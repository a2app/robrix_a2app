//! Host-owned image requests: credentials, recipients and approval never enter workers.

use std::{sync::{Arc, atomic::{AtomicBool, Ordering}}, time::Duration};
use a2app_core::information_flow::{self as flow, ContextId, Influence, Recipient};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use super::ModelRecipient;

const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 24 * 1024 * 1024;

/// Deliberately has no Debug implementation: it owns a provider credential.
pub struct ResolvedImage {
    pub recipient: ModelRecipient,
    pub provider: String,
    pub model: String,
    key: String,
}

pub struct GeneratedImage { pub bytes: Vec<u8>, pub mime_type: String }

fn endpoint(base: &str, provider: &str, model: &str) -> Result<(String, bool), String> {
    let mut url = url::Url::parse(base).map_err(|_| "Invalid image service endpoint.")?;
    let local = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(), Some(url::Host::Ipv6(ip)) => ip.is_loopback(), _ => false,
    };
    if (url.scheme() != "https" && !(url.scheme() == "http" && local)) || url.host_str().is_none()
        || !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return Err("Image APIs require HTTPS, or a literal loopback address, without URL credentials or query parameters.".into());
    }
    let base = url.path().trim_end_matches('/');
    // Only Google's documented compatibility endpoint maps to its native API.
    // Custom endpoints retain their complete configured path.
    let base = if provider == "gemini" && url.host_str() == Some("generativelanguage.googleapis.com")
        && base == "/v1beta/openai" { "/v1beta" } else { base };
    let path = if provider == "openai" { format!("{base}/images/generations") }
        else { format!("{base}/models/{model}:generateContent") };
    url.set_path(&path);
    Ok((url.into(), local))
}

fn resolve_service(provider: Option<&str>) -> Result<(String, String, String), String> {
    if let Some(provider) = provider {
        if !matches!(provider, "openai" | "gemini") {
            return Err("Image generation supports OpenAI and Gemini. Choose a configured provider.".into());
        }
        let (base, key) = super::guarded::image_service_config(provider)?;
        return Ok((provider.into(), base, key));
    }
    let preferred = crate::providers::effective_provider().filter(|id| matches!(id.as_str(), "openai" | "gemini"));
    let mut unavailable = None;
    if let Some(provider) = &preferred {
        match super::guarded::image_service_config(provider) {
            Ok((base, key)) => return Ok((provider.clone(), base, key)),
            Err(error) => unavailable = Some(error),
        }
    }
    // A remembered selection can outlive its credential. Only automatic
    // selection may use another configured provider, before its effect review.
    for provider in crate::providers::list() {
        if !matches!(provider.id.as_str(), "openai" | "gemini") || preferred.as_ref() == Some(&provider.id) { continue; }
        match super::guarded::image_service_config(&provider.id) {
            Ok((base, key)) => return Ok((provider.id, base, key)),
            Err(error) => { if unavailable.is_none() { unavailable = Some(error); } }
        }
    }
    Err(unavailable.unwrap_or_else(|| "Image generation needs an OpenAI or Gemini API key. Add one in Setup AI Providers; your chat provider can stay selected.".into()))
}

pub fn resolve(provider: Option<&str>, model: Option<&str>) -> Result<ResolvedImage, String> {
    let (provider, base, key) = resolve_service(provider)?;
    let model = model.unwrap_or(if provider == "openai" { "gpt-image-1.5" } else { "gemini-2.5-flash-image" });
    if model.is_empty() || model.len() > 128 || !model.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')) {
        return Err("Invalid image model id.".into());
    }
    let (endpoint, local) = endpoint(&base, &provider, model)?;
    let id = super::guarded::identity(&super::guarded::identity_salt()?, &[&provider, &endpoint, model, "image-generation", &key]);
    Ok(ResolvedImage { recipient: ModelRecipient { id, label: format!("{} / {model}", crate::providers::label_for(&provider)), endpoint, local },
        provider, model: model.into(), key })
}

pub fn review_payload(config: &ResolvedImage, prompt: &str) -> Value {
    json!({"operation":"image.generate","provider_label":config.recipient.label,"model":config.model,"prompt":prompt})
}

fn body(config: &ResolvedImage, prompt: &str) -> Value {
    if config.provider == "openai" {
        // GPT image models always return base64. Legacy response_format is unsupported.
        json!({"model":config.model,"prompt":prompt,"n":1,"size":"1024x1024","output_format":"png"})
    } else {
        json!({"contents":[{"parts":[{"text":prompt}]}],"generationConfig":{"responseModalities":["TEXT","IMAGE"]}})
    }
}

fn decode(value: Value, provider: &str) -> Result<GeneratedImage, String> {
    let (data, mime_type) = if provider == "openai" {
        (value["data"][0]["b64_json"].as_str().ok_or("The image service returned no image bytes. It may have declined the request; nothing was drafted or sent.")?, "image/png")
    } else {
        let parts = value["candidates"][0]["content"]["parts"].as_array().ok_or("The image service returned no image.")?;
        let data = parts.iter().find_map(|part| part.get("inlineData").or_else(|| part.get("inline_data")))
            .ok_or("The image service returned text without an image; nothing was drafted or sent.")?;
        (data["data"].as_str().ok_or("The generated image data is missing.")?,
            data["mimeType"].as_str().or_else(|| data["mime_type"].as_str()).ok_or("The generated image type is missing.")?)
    };
    if data.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 || !matches!(mime_type, "image/png" | "image/jpeg" | "image/webp") {
        return Err("The generated image exceeds the attachment limit or has an unsupported image format.".into());
    }
    let bytes = STANDARD.decode(data).map_err(|_| "The generated image is not valid base64.")?;
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES { return Err("The generated image is empty or exceeds the attachment limit.".into()); }
    Ok(GeneratedImage { bytes, mime_type: mime_type.into() })
}

/// Caller obtains the immutable effect review first. Commit consumes a one-time
/// grant just before sending; cancellation and source activation remain live.
pub async fn generate(config: &ResolvedImage, context: ContextId, epoch: u64, alive: Arc<AtomicBool>, prompt: &str) -> Result<GeneratedImage, String> {
    let recipient = Recipient::ModelProvider(config.recipient.id.clone());
    let payload = review_payload(config, prompt);
    let check = || {
        if !alive.load(Ordering::Acquire) { return Err("Image generation was cancelled.".to_string()); }
        flow::ensure_context_epoch(&context, epoch)
    };
    check()?;
    if resolve(Some(&config.provider), Some(&config.model))?.recipient.id != config.recipient.id {
        return Err("Image provider configuration changed. Review the current provider before trying again.".into());
    }
    flow::check_effect_for_activation(&context, epoch, Some(&recipient), None, &payload)?;
    let attempt = a2app_core::protection_audit::Attempt::start(&context, Some(recipient.clone()), a2app_core::protection_audit::ActivityKind::ModelRequest);
    let result = send(config, prompt, check, || flow::commit_effect_for_activation(&context, epoch, Some(&recipient), None, &payload)).await;
    attempt.finish(result.is_ok());
    check()?;
    flow::add_influences_for_activation(&context, epoch, [Influence::Model(config.recipient.id.clone())])?;
    if resolve(Some(&config.provider), Some(&config.model))?.recipient.id != config.recipient.id {
        return Err("Image provider configuration changed while generating. Try again with the current provider.".into());
    }
    result
}

async fn send(config: &ResolvedImage, prompt: &str, check: impl Fn() -> Result<(), String>, commit: impl FnOnce() -> Result<(), String>) -> Result<GeneratedImage, String> {
    check()?;
    let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).no_proxy().referer(false)
        .retry(reqwest::retry::never()).timeout(Duration::from_secs(180)).build().map_err(|_| "Cannot initialize image generation.")?;
    let request = client.post(&config.recipient.endpoint).json(&body(config, prompt));
    let request = if config.provider == "openai" { request.bearer_auth(&config.key) } else { request.header("x-goog-api-key", &config.key) };
    check()?;
    commit()?;
    let receive = async {
        let mut response = request.send().await.map_err(|_| "Image generation transport failed.")?;
        if !response.status().is_success() {
            return Err(format!("Image generation returned HTTP {}. Check this provider's image-model access and quota in Setup AI Providers; nothing was drafted or sent.", response.status().as_u16()));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| "The generated image response could not be read.")? {
            if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES { return Err("Image generation response exceeded its size limit.".into()); }
            bytes.extend_from_slice(&chunk);
        }
        let value = serde_json::from_slice(&bytes).map_err(|_| "Image generation returned invalid JSON.")?;
        decode(value, &config.provider)
    };
    tokio::pin!(receive);
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    let result = loop {
        tokio::select! {
            biased;
            _ = interval.tick() => if let Err(error) = check() { break Err(error); },
            result = &mut receive => break result,
        }
    };
    check()?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::{Read, Write}, net::TcpListener};

    #[test]
    fn automatic_selection_skips_unavailable_credentials_without_bypassing_explicit_selection() {
        let _guard = crate::CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        struct Restore { env: Vec<(&'static str, Option<std::ffi::OsString>)>, provider: Option<String> }
        impl Drop for Restore {
            fn drop(&mut self) {
                for (name, value) in &self.env {
                    unsafe { match value {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    } }
                }
                match &self.provider { Some(value) => crate::providers::set_session(value), None => crate::providers::clear_session() }
            }
        }
        let _restore = Restore {
            env: ["OCTOS_CONFIG_DIR", "ROBRIX_TEST_IMAGE_CREDENTIAL"].into_iter()
                .map(|name| (name, std::env::var_os(name))).collect(),
            provider: crate::providers::session_provider(),
        };
        let directory = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("OCTOS_CONFIG_DIR", directory.path()); std::env::remove_var("ROBRIX_TEST_IMAGE_CREDENTIAL"); }
        crate::providers::set_session("openai");
        let mut config = json!({"provider":"openai","base_url":"https://proxy.example/v1/openai",
            "api_key_env":"ROBRIX_TEST_IMAGE_CREDENTIAL",
            "env_vars":{"OPENAI_API_KEY":"unrelated-openai-key","GEMINI_API_KEY":"fallback-gemini-key"}});
        let write = |value: &Value| std::fs::write(directory.path().join("config.json"), value.to_string()).unwrap();
        write(&config);
        let fallback = resolve(None, None).unwrap();
        assert_eq!(fallback.provider, "gemini");
        assert_eq!(fallback.key, "fallback-gemini-key");
        assert!(fallback.recipient.endpoint.starts_with("https://generativelanguage.googleapis.com/"),
            "a fallback provider must not inherit the selected provider's custom endpoint");
        assert!(resolve(Some("openai"), None).is_err(),
            "an explicit custom credential remains exclusive despite another available provider and ordinary OpenAI key");
        config["env_vars"]["ROBRIX_TEST_IMAGE_CREDENTIAL"] = json!("preferred-custom-key");
        write(&config);
        let preferred = resolve(None, None).unwrap();
        assert_eq!(preferred.provider, "openai");
        assert_eq!(preferred.key, "preferred-custom-key");
        assert_eq!(preferred.recipient.endpoint, "https://proxy.example/v1/openai/images/generations");
    }

    #[tokio::test]
    #[ignore = "uses a configured image provider and its quota; contains only a synthetic public prompt"]
    async fn live_generation_produces_an_actual_image_file() {
        let config = resolve(None, None).expect("Configure an OpenAI or Gemini image provider first");
        let context = ContextId::Agent { account:"public-image-generation-smoke".into(), room:"!public-smoke:example.invalid".into() };
        flow::register_context(&context).unwrap();
        let prompt = "A simple test illustration on a white background: one red circle on the left and one blue square on the right. No text.";
        let epoch = flow::context_epoch(&context).unwrap();
        let recipient = Recipient::ModelProvider(config.recipient.id.clone());
        let review = flow::prepare_effect_for_activation(&context, epoch, Some(&recipient), None, &review_payload(&config, prompt)).unwrap();
        assert!(review.allowed, "this isolated test has no private source data");
        let result = generate(&config, context.clone(), epoch, Arc::new(AtomicBool::new(true)), prompt).await;
        flow::remove_context(&context).unwrap();
        let image = result.unwrap();
        assert!(image.bytes.starts_with(b"\x89PNG\r\n\x1a\n") || image.bytes.starts_with(&[0xff, 0xd8])
            || (image.bytes.starts_with(b"RIFF") && image.bytes.get(8..12) == Some(b"WEBP")), "the provider must return an image file");
        let path = a2app_core::data_root().join("live-generated-image.bin");
        std::fs::create_dir_all(a2app_core::data_root()).unwrap();
        std::fs::write(&path, &image.bytes).unwrap();
        eprintln!("Generated {} bytes of {} via {}; artifact {}", image.bytes.len(), image.mime_type, config.recipient.label, path.display());
    }

    #[tokio::test]
    async fn replaced_activation_cannot_generate_under_its_successor() {
        let context = ContextId::Agent { account:"image-generation-replacement".into(), room:"!image-generation-replacement:example.invalid".into() };
        flow::register_context(&context).unwrap();
        let original = flow::context_epoch(&context).unwrap();
        flow::remove_context(&context).unwrap();
        flow::register_context(&context).unwrap();
        let replacement = flow::context_epoch(&context).unwrap();
        assert_ne!(original, replacement);
        let config = ResolvedImage { recipient:ModelRecipient { id:"fixture".into(), label:"Fixture".into(),
            endpoint:"https://generation.invalid/v1/images/generations".into(), local:false },
            provider:"openai".into(), model:"fixture-image".into(), key:"fixture".into() };
        let result = generate(&config, context.clone(), original, Arc::new(AtomicBool::new(true)), "old prompt").await;
        assert!(result.is_err(), "an old request must fail before reading configuration or using the network");
        flow::ensure_context_epoch(&context, replacement).unwrap();
        flow::remove_context(&context).unwrap();
    }

    #[tokio::test]
    async fn actual_image_request_is_authorized_and_returns_decoded_bytes() {
        for provider in ["openai", "gemini"] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}/v1", listener.local_addr().unwrap());
            let config = ResolvedImage { recipient: ModelRecipient { id:"fixture".into(), label:"Fixture".into(),
                endpoint:endpoint(&base, provider, "fixture-image").unwrap().0, local:true },
                provider:provider.into(),model:"fixture-image".into(),key:"fixture-key".into() };
            let image_bytes = include_bytes!("../../../../resources/icon_32.png");
            let encoded = STANDARD.encode(image_bytes);
            let response = if provider == "openai" { json!({"data":[{"b64_json":encoded}]}) }
                else { json!({"candidates":[{"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":encoded}}]}}]}) }.to_string();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let count = stream.read(&mut chunk).unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&chunk[..count]);
                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
                        let length: usize = headers.lines().find_map(|line| line.strip_prefix("content-length: ")).unwrap().parse().unwrap();
                        if request.len() >= end + 4 + length { break; }
                    }
                }
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
                String::from_utf8(request).unwrap()
            });
            let committed = AtomicBool::new(false);
            let result = send(&config, "Draw a rowing crew", || Ok(()), || { committed.store(true, Ordering::Release); Ok(()) }).await.unwrap();
            assert!(committed.load(Ordering::Acquire));
            assert_eq!(result.bytes, image_bytes);
            assert!(result.bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
            let request = server.join().unwrap();
            assert!(request.starts_with("POST /v1/"));
            assert!(request.contains("Draw a rowing crew"));
            if provider == "openai" { assert!(request.contains("authorization: Bearer fixture-key")); assert!(!request.contains("response_format")); }
            else { assert!(request.contains("x-goog-api-key: fixture-key")); assert!(request.contains("responseModalities")); }
        }
    }

    #[tokio::test]
    async fn denied_generation_opens_no_connection_and_does_not_consume_authority() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let config = ResolvedImage { recipient:ModelRecipient { id:"fixture".into(),label:"Fixture".into(),
            endpoint:format!("http://{}/v1/images/generations", listener.local_addr().unwrap()),local:true },
            provider:"openai".into(),model:"fixture-image".into(),key:"fixture".into() };
        assert!(send(&config, "prompt", || Err("denied".into()), || panic!("a denied request cannot commit")).await.is_err());
        assert!(matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock));
    }
    #[test]
    fn endpoints_keep_the_configured_origin_and_do_not_put_keys_in_urls() {
        assert_eq!(endpoint("https://api.openai.com/v1", "openai", "gpt-image-1.5").unwrap().0, "https://api.openai.com/v1/images/generations");
        assert_eq!(endpoint("https://generativelanguage.googleapis.com/v1beta/openai", "gemini", "gemini-2.5-flash-image").unwrap().0,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash-image:generateContent");
        assert_eq!(endpoint("https://proxy.example/v1/openai", "openai", "gpt-image-1.5").unwrap().0,
            "https://proxy.example/v1/openai/images/generations");
        assert_eq!(endpoint("https://proxy.example/v1/openai", "gemini", "gemini-2.5-flash-image").unwrap().0,
            "https://proxy.example/v1/openai/models/gemini-2.5-flash-image:generateContent");
        assert!(endpoint("https://user:secret@example.com/v1", "openai", "image").is_err());
        assert!(endpoint("https://example.com/v1?key=secret", "openai", "image").is_err());
        assert!(endpoint("http://example.com/v1", "openai", "image").is_err());
    }
    #[test]
    fn image_responses_extract_bytes_instead_of_returning_metadata_or_remote_urls() {
        let encoded = STANDARD.encode(b"image bytes");
        let openai = decode(json!({"data":[{"b64_json":encoded}]}), "openai").unwrap();
        assert_eq!(openai.bytes, b"image bytes");
        assert_eq!(openai.mime_type, "image/png");
        let gemini = decode(json!({"candidates":[{"content":{"parts":[{"text":"Here is the image"},{"inlineData":{"mimeType":"image/webp","data":encoded}}]}}]}), "gemini").unwrap();
        assert_eq!(gemini.bytes, b"image bytes");
        assert_eq!(gemini.mime_type, "image/webp");
        assert!(decode(json!({"data":[{"url":"https://external.invalid/image.png"}]}), "openai").is_err());
        assert!(decode(json!({"data":[{"b64_json":"invalid!"}]}), "openai").is_err());
        assert!(decode(json!({"candidates":[{"content":{"parts":[{"text":"No image"}]}}]}), "gemini").is_err());
    }
}
