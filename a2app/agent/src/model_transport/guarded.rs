use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::time::Duration;

use a2app_core::information_flow::{self, ContextId, Influence, Recipient};
use octos_core::{Message, MessageRole, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolChoice, ToolSpec};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{AgentPrefs, ModelRecipient, stored_api_key};

#[derive(Clone, Copy, PartialEq)]
enum Protocol { OpenAi, Anthropic }

/// The model configuration fields consumed by the host transport.
///
/// Keep credential resolution compatible with Octos without linking its CLI,
/// tool implementations, or agent runtime into the subprocess-only host.
#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct ModelConfig {
    provider: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    api_key_env: Option<String>,
    env_vars: std::collections::HashMap<String, String>,
    model_hints: Option<octos_llm::openai::ModelHints>,
    api_type: Option<String>,
}

impl ModelConfig {
    fn api_key(&self, provider: &str) -> Option<String> {
        let entry = octos_llm::registry::lookup(provider);
        let default_name = entry.and_then(|entry| entry.api_key_env).map(str::to_string)
            .unwrap_or_else(|| format!("{}_API_KEY", provider.to_uppercase()));
        let name = self.api_key_env.as_deref().unwrap_or(&default_name);
        let known = entry.map_or(name == default_name, |entry| entry.is_known_key_env(name));
        // An explicit custom variable is exclusive: an ambient provider login
        // must never substitute a different credential for a proxy endpoint.
        let provider_chain = self.api_key_env.is_none() || known;
        let mut candidates = vec![name];
        if known {
            if let Some(entry) = entry {
                for sibling in entry.key_env_names() {
                    if !candidates.contains(&sibling) { candidates.push(sibling); }
                }
            }
        }
        if provider_chain {
            if let Some(key) = stored_api_key(provider) { return Some(key); }
        }
        for name in &candidates {
            if let Some(value) = self.env_vars.get(*name).and_then(|value| resolve_config_secret(name, value)) {
                if !value.is_empty() { return Some(value); }
            }
        }
        candidates.into_iter().find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
    }
}

fn resolve_config_secret(name: &str, value: &str) -> Option<String> {
    let Some(account) = value.strip_prefix("keychain:") else { return Some(value.into()); };
    let account = if account.is_empty() { name } else { account };
    #[cfg(target_os = "macos")]
    {
        use std::io::Read;
        use std::process::Stdio;
        let mut child = std::process::Command::new("/usr/bin/security")
            .args(["find-generic-password", "-s", "octos", "-a", account, "-w"])
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null())
            .spawn().ok()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        // Keep ownership until reaped: a timed-out keychain lookup must not
        // leave a detached process/thread behind on every model request.
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => break,
                Ok(Some(_)) => return None,
                Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                _ => { let _ = child.kill(); let _ = child.wait(); return None; }
            }
        }
        let mut bytes = Vec::new();
        child.stdout.take()?.take(64 * 1024).read_to_end(&mut bytes).ok()?;
        if bytes.len() == 64 * 1024 { return None; }
        let value = String::from_utf8(bytes).ok()?.trim().to_string();
        Some(decode_keychain_hex(&value).unwrap_or(value))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = account;
        None
    }
}

// macOS prints multiline secrets as hex; ordinary hex-shaped keys stay literal.
#[cfg(any(target_os = "macos", test))]
fn decode_keychain_hex(value: &str) -> Option<String> {
    if value.len() < 2 || value.len() % 2 != 0 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) { return None; }
    let bytes: Option<Vec<u8>> = (0..value.len()).step_by(2).map(|index| u8::from_str_radix(&value[index..index + 2], 16).ok()).collect();
    let decoded = String::from_utf8(bytes?).ok()?;
    decoded.contains('\n').then_some(decoded)
}

// Deliberately no Debug: the credential never belongs in diagnostics.
pub(super) struct Resolved {
    pub(super) recipient: ModelRecipient,
    provider: String,
    model: String,
    key: String,
    protocol: Protocol,
    hints: octos_llm::openai::ModelHints,
}

fn endpoint(base: &str, protocol: Protocol) -> Result<(String, bool), String> {
    let mut url = url::Url::parse(base).map_err(|_| "Invalid model API endpoint.")?;
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return Err("Model API endpoints cannot contain credentials, queries, or fragments.".into());
    }
    let local = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    if url.scheme() != "https" && !(url.scheme() == "http" && local) {
        return Err("Use HTTPS for model APIs, or a literal loopback IP for a local HTTP service.".into());
    }
    let base_path = url.path().trim_end_matches('/');
    let path = match protocol {
        Protocol::OpenAi => format!("{base_path}/chat/completions"),
        Protocol::Anthropic if base_path.ends_with("/v1") => format!("{base_path}/messages"),
        Protocol::Anthropic => format!("{base_path}/v1/messages"),
    };
    url.set_path(&path);
    Ok((url.to_string(), local))
}

fn identity(salt: &[u8], fields: &[&str]) -> String {
    let mut digest = Sha256::new();
    digest.update(salt);
    for field in fields {
        digest.update((field.len() as u64).to_be_bytes());
        digest.update(field.as_bytes());
    }
    format!("model:{:x}", digest.finalize())
}

fn identity_salt() -> Result<Vec<u8>, String> {
    use std::io::Write;
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().map_err(|_| "Model identity storage unavailable.")?;
    let root = a2app_core::data_root();
    std::fs::create_dir_all(root).map_err(|_| "Cannot create model identity storage.")?;
    let path = root.join("model_recipient_salt");
    if path.exists() {
        let bytes = std::fs::read(path).map_err(|_| "Cannot read model identity storage.")?;
        return if bytes.len() == 32 { Ok(bytes) } else { Err("Invalid model identity storage.".into()) };
    }
    let mut salt = vec![0u8; 32];
    getrandom::fill(&mut salt).map_err(|_| "Cannot create a private model identity.")?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|_| "Cannot save model identity storage.")?;
    file.write_all(&salt).and_then(|_| file.sync_all()).map_err(|_| "Cannot save model identity storage.")?;
    Ok(salt)
}

pub(super) fn resolve(prefs: &AgentPrefs) -> Result<Resolved, String> {
    let config: ModelConfig = match crate::octos_config_candidates().into_iter().find(|path| path.exists()) {
        Some(path) => serde_json::from_str(&std::fs::read_to_string(path).map_err(|_| "Cannot read model configuration.")?)
            .map_err(|_| "Cannot parse model configuration.")?,
        None => ModelConfig::default(),
    };
    let selected = crate::providers::session_provider().or_else(|| config.provider.clone())
        .or_else(|| crate::provider_from_env().map(str::to_string))
        .or_else(crate::provider_from_auth_store)
        .ok_or("Choose a model provider before sharing private data.")?;
    let entry = octos_llm::registry::lookup(&selected);
    let name = entry.map(|entry| entry.name).or_else(|| selected.eq_ignore_ascii_case("custom").then_some("custom"))
        .ok_or("This model backend cannot enforce private-data sharing. Select an OpenAI-compatible or Anthropic provider.")?;
    if !matches!(name, "openai" | "anthropic" | "deepseek" | "moonshot" | "moonshot-coding" | "groq" | "openrouter" | "ollama" | "custom") {
        return Err("This model backend is not supported by the guarded transport. Select an OpenAI-compatible or Anthropic provider.".into());
    }
    let protocol = match config.api_type.as_deref() {
        Some("anthropic") => Protocol::Anthropic,
        Some("openai") => Protocol::OpenAi,
        Some(_) => return Err("Unsupported model API protocol. Choose openai or anthropic.".into()),
        None if name == "anthropic" => Protocol::Anthropic,
        None => Protocol::OpenAi,
    };
    let base = config.base_url.as_deref().or_else(|| entry.and_then(|entry| entry.default_base_url))
        .ok_or("Configure a model API base URL.")?;
    // Only the built-in Ollama default is rewritten. Arbitrary HTTP DNS names
    // do not become trusted local services because of a user-supplied label.
    let base = if name == "ollama" && config.base_url.is_none() { "http://127.0.0.1:11434/v1" } else { base };
    let (endpoint, local) = endpoint(base, protocol)?;
    let model = crate::prefs::Backend::Octos { provider: name.into() }.model_override(prefs)
        .or_else(|| config.model.clone()).or_else(|| entry.and_then(|entry| entry.default_model).map(str::to_string))
        .ok_or("Choose a model for the configured provider.")?;
    let key = if entry.is_some_and(|entry| entry.requires_api_key) || config.api_key_env.is_some() { config.api_key(&selected).ok_or("Configure the API credential for this model provider.")? } else { String::new() };
    let protocol_name = if protocol == Protocol::OpenAi { "openai" } else { "anthropic" };
    let id = identity(&identity_salt()?, &[name, &endpoint, &model, protocol_name, &key]);
    let hints = config.model_hints.unwrap_or_else(|| octos_llm::openai::ModelHints::detect(&model));
    Ok(Resolved {
        recipient: ModelRecipient { id, label: format!("{} / {}", name, model), endpoint, local },
        provider: name.into(), model, key, protocol, hints,
    })
}

type Approval = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

pub(crate) fn provider(prefs: &AgentPrefs, context: ContextId, shutdown: Arc<AtomicBool>) -> Result<Arc<dyn LlmProvider>, String> {
    let epoch = information_flow::context_epoch(&context)?;
    let config = resolve(prefs)?;
    let prefs = prefs.clone();
    let expected = config.recipient.id.clone();
    let recipient = Recipient::ModelProvider(expected.clone());
    let check_config: Approval = Arc::new(move || {
        if resolve(&prefs)?.recipient.id != expected {
            return Err("Model configuration changed. Restart the agent and approve the current model service.".into());
        }
        Ok(())
    });
    let response_context = context.clone();
    let audit_context = context.clone();
    let response_model = config.recipient.id.clone();
    let approve: Approval = Arc::new(move || {
        if shutdown.load(Ordering::Acquire) { return Err("Model request cancelled.".into()); }
        information_flow::ensure_allowed_for_activation(&context, epoch, &recipient)?;
        Ok(())
    });
    let mut provider = GuardedProvider::new(config, approve)?;
    provider.audit_context = Some(audit_context);
    provider.check_config = Some(check_config);
    provider.on_response = Some(Arc::new(move || {
        information_flow::add_influences_for_activation(&response_context, epoch, [Influence::Model(response_model.clone())])
    }));
    Ok(Arc::new(provider))
}

struct GuardedProvider { config: Resolved, client: reqwest::Client, approve: Approval, check_config: Option<Approval>, on_response: Option<Approval>, audit_context: Option<ContextId> }

impl GuardedProvider {
    fn new(config: Resolved, approve: Approval) -> Result<Self, String> {
        let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none())
            .no_proxy().referer(false).retry(reqwest::retry::never()).http1_only()
            .no_gzip().no_brotli().no_deflate().no_zstd()
            .timeout(Duration::from_secs(180)).build()
            .map_err(|_| "Cannot initialize the guarded model transport.")?;
        Ok(Self { config, client, approve, check_config: None, on_response: None, audit_context: None })
    }

    async fn send(&self, body: Value) -> eyre::Result<Value> {
        (self.approve)().map_err(eyre::Report::msg)?;
        if let Some(check) = &self.check_config { check().map_err(eyre::Report::msg)?; }
        (self.approve)().map_err(eyre::Report::msg)?;
        let mut request = self.client.post(&self.config.recipient.endpoint).json(&body);
        if !self.config.key.is_empty() {
            request = match self.config.protocol {
                Protocol::OpenAi => request.bearer_auth(&self.config.key),
                Protocol::Anthropic => request.header("x-api-key", &self.config.key),
            };
        }
        if self.config.protocol == Protocol::Anthropic { request = request.header("anthropic-version", "2023-06-01"); }
        // Re-check while a request is in flight as well as between model calls.
        // Revocation cannot recall bytes already delivered while authorized.
        let response = async {
            let attempt = self.audit_context.as_ref().map(|context| a2app_core::protection_audit::Attempt::start(context,
                Some(Recipient::ModelProvider(self.config.recipient.id.clone())), a2app_core::protection_audit::ActivityKind::ModelRequest));
            let result = async {
                let mut response = request.send().await.map_err(|_| eyre::eyre!("Model transport failed."))?;
                if !response.status().is_success() {
                    return Err(eyre::eyre!("Model service returned HTTP {}. Redirects are not followed.", response.status().as_u16()));
                }
                let mut bytes = Vec::new();
                while let Some(chunk) = response.chunk().await.map_err(|_| eyre::eyre!("Model response could not be read."))? {
                    if bytes.len() + chunk.len() > 16 * 1024 * 1024 { return Err(eyre::eyre!("Model response exceeded the size limit.")); }
                    bytes.extend_from_slice(&chunk);
                }
                serde_json::from_slice(&bytes).map_err(|_| eyre::eyre!("Model service returned an invalid response."))
            }.await;
            if let Some(attempt) = attempt { attempt.finish(result.is_ok()); }
            result
        };
        tokio::pin!(response);
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        loop {
            tokio::select! {
                biased;
                _ = interval.tick() => { (self.approve)().map_err(eyre::Report::msg)?; }
                result = &mut response => {
                    (self.approve)().map_err(eyre::Report::msg)?;
                    return result;
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for GuardedProvider {
    async fn chat(&self, messages: &[Message], tools: &[ToolSpec], config: &ChatConfig) -> eyre::Result<ChatResponse> {
        (self.approve)().map_err(eyre::Report::msg)?;
        if messages.iter().any(|message| !message.media.is_empty()) {
            return Err(eyre::eyre!("Media inputs need explicit source tracking and are not supported by the guarded model transport yet."));
        }
        let body = match self.config.protocol {
            Protocol::OpenAi => openai_body(&self.config, messages, tools, config),
            Protocol::Anthropic => anthropic_body(&self.config, messages, tools, config),
        };
        let result = self.send(body).await;
        // Service failures can influence retries too. Persist the influence
        // before returning either an error, text, or proposed tool calls.
        if let Some(record) = &self.on_response { record().map_err(eyre::Report::msg)?; }
        let value = result?;
        match self.config.protocol {
            Protocol::OpenAi => parse_openai(value),
            Protocol::Anthropic => parse_anthropic(value),
        }
    }
    fn model_id(&self) -> &str { &self.config.model }
    fn provider_name(&self) -> &str { &self.config.provider }
}

fn openai_body(resolved: &Resolved, messages: &[Message], tools: &[ToolSpec], config: &ChatConfig) -> Value {
    let messages: Vec<_> = messages.iter().map(|message| {
        let mut value = json!({"role": message.role.as_str(), "content": message.content});
        if let Some(id) = &message.tool_call_id { value["tool_call_id"] = json!(id); }
        if let Some(reasoning) = &message.reasoning_content { value["reasoning_content"] = json!(reasoning); }
        if let Some(calls) = &message.tool_calls {
            value["tool_calls"] = json!(calls.iter().map(|call| json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments.to_string()}})).collect::<Vec<_>>());
        }
        value
    }).collect();
    let mut value = json!({"model":resolved.model,"messages":messages,"stream":false});
    if let Some(max) = config.max_tokens { value[if resolved.hints.uses_completion_tokens { "max_completion_tokens" } else { "max_tokens" }] = json!(max); }
    if !resolved.hints.fixed_temperature {
        if let Some(temperature) = config.temperature { value["temperature"] = json!(temperature); }
    }
    if !config.stop_sequences.is_empty() { value["stop"] = json!(config.stop_sequences); }
    if !tools.is_empty() {
        value["tools"] = json!(tools.iter().map(|tool| json!({"type":"function","function":{"name":tool.name,"description":tool.description,"parameters":tool.input_schema}})).collect::<Vec<_>>());
        value["tool_choice"] = match &config.tool_choice {
            ToolChoice::Auto => json!("auto"), ToolChoice::None => json!("none"), ToolChoice::Required => json!("required"),
            ToolChoice::Specific { name } => json!({"type":"function","function":{"name":name}}),
        };
    }
    value
}

fn anthropic_body(resolved: &Resolved, messages: &[Message], tools: &[ToolSpec], config: &ChatConfig) -> Value {
    let mut system = Vec::new();
    let mut turns: Vec<Value> = Vec::new();
    for message in messages {
        if message.role == MessageRole::System { system.push(message.content.clone()); continue; }
        let role = if message.role == MessageRole::Assistant { "assistant" } else { "user" };
        let mut content = Vec::new();
        if message.role == MessageRole::Tool {
            content.push(json!({"type":"tool_result","tool_use_id":message.tool_call_id,"content":message.content}));
        } else {
            if !message.content.is_empty() { content.push(json!({"type":"text","text":message.content})); }
            for call in message.tool_calls.iter().flatten() {
                content.push(json!({"type":"tool_use","id":call.id,"name":call.name,"input":call.arguments}));
            }
        }
        if let Some(previous) = turns.last_mut().filter(|previous| previous["role"] == role) {
            previous["content"].as_array_mut().unwrap().extend(content);
        } else { turns.push(json!({"role":role,"content":content})); }
    }
    let mut value = json!({"model":resolved.model,"system":system.join("\n\n"),"messages":turns,"max_tokens":config.max_tokens.unwrap_or(8192),"stream":false});
    if let Some(temperature) = config.temperature { value["temperature"] = json!(temperature); }
    if !config.stop_sequences.is_empty() { value["stop_sequences"] = json!(config.stop_sequences); }
    if !tools.is_empty() {
        value["tools"] = json!(tools);
        value["tool_choice"] = match &config.tool_choice {
            ToolChoice::Auto => json!({"type":"auto"}), ToolChoice::None => json!({"type":"none"}),
            ToolChoice::Required => json!({"type":"any"}), ToolChoice::Specific { name } => json!({"type":"tool","name":name}),
        };
    }
    value
}

fn number(value: &Value) -> u32 { value.as_u64().unwrap_or(0).min(u32::MAX as u64) as u32 }
fn required(value: &Value) -> eyre::Result<String> {
    value.as_str().map(str::to_string).ok_or_else(|| eyre::eyre!("Malformed model response."))
}
fn parse_openai(value: Value) -> eyre::Result<ChatResponse> {
    let choice = value["choices"].as_array().and_then(|choices| choices.first()).ok_or_else(|| eyre::eyre!("Missing model response."))?;
    let message = &choice["message"];
    let mut calls = Vec::new();
    for call in message["tool_calls"].as_array().into_iter().flatten() {
        calls.push(ToolCall { id: required(&call["id"])?, name: required(&call["function"]["name"])?,
            arguments: serde_json::from_str(&required(&call["function"]["arguments"])?).map_err(|_| eyre::eyre!("Malformed model tool arguments."))?, metadata: None });
    }
    let usage = &value["usage"];
    let cached = number(&usage["prompt_tokens_details"]["cached_tokens"]);
    Ok(ChatResponse {
        content: message["content"].as_str().map(str::to_string), reasoning_content: message["reasoning_content"].as_str().map(str::to_string),
        stop_reason: match choice["finish_reason"].as_str() { Some("tool_calls") => StopReason::ToolUse, Some("length") => StopReason::MaxTokens, Some("content_filter") => StopReason::ContentFiltered, _ => StopReason::EndTurn },
        tool_calls: calls, usage: TokenUsage { input_tokens: number(&usage["prompt_tokens"]).saturating_sub(cached), output_tokens: number(&usage["completion_tokens"]), cache_read_tokens: cached, ..Default::default() }, provider_index: None,
    })
}
fn parse_anthropic(value: Value) -> eyre::Result<ChatResponse> {
    let blocks = value["content"].as_array().ok_or_else(|| eyre::eyre!("Missing model response."))?;
    let mut text = String::new();
    let mut calls = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => text.push_str(&required(&block["text"])?),
            Some("tool_use") => calls.push(ToolCall { id: required(&block["id"])?, name: required(&block["name"])?, arguments: block["input"].clone(), metadata: None }),
            _ => {}
        }
    }
    let usage = &value["usage"];
    Ok(ChatResponse {
        content: (!text.is_empty()).then_some(text), reasoning_content: None, tool_calls: calls,
        stop_reason: match value["stop_reason"].as_str() { Some("tool_use") => StopReason::ToolUse, Some("max_tokens") => StopReason::MaxTokens, Some("stop_sequence") => StopReason::StopSequence, _ => StopReason::EndTurn },
        usage: TokenUsage { input_tokens: number(&usage["input_tokens"]), output_tokens: number(&usage["output_tokens"]), cache_read_tokens: number(&usage["cache_read_input_tokens"]), cache_write_tokens: number(&usage["cache_creation_input_tokens"]), ..Default::default() }, provider_index: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn fixture(base: &str, protocol: Protocol) -> Resolved {
        let (endpoint, local) = endpoint(base, protocol).unwrap();
        Resolved {
            recipient: ModelRecipient { id: "test-recipient".into(), label: "Test model".into(), endpoint, local },
            provider: "openai".into(), model: "test-model".into(), key: "fixture-credential".into(),
            protocol, hints: octos_llm::openai::ModelHints::default(),
        }
    }
    fn message(role: MessageRole, content: &str) -> Message {
        Message { role, content: content.into(), media: vec![], tool_calls: None, tool_call_id: None,
            reasoning_content: None, client_message_id: None, thread_id: None, timestamp: chrono::Utc::now() }
    }
    fn approval(allowed: Arc<AtomicBool>) -> Approval {
        Arc::new(move || if allowed.load(Ordering::SeqCst) { Ok(()) } else { Err("Private source policy denied this recipient.".into()) })
    }
    fn request(listener: TcpListener, response: String, revoke: Option<Arc<AtomicBool>>) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0; 4096];
            loop {
                let count = stream.read(&mut chunk).unwrap();
                if count == 0 { break; }
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                    let len = header.lines().find_map(|line| line.strip_prefix("content-length: ")).unwrap().parse::<usize>().unwrap();
                    if bytes.len() >= end + 4 + len { break; }
                }
            }
            if let Some(allowed) = revoke {
                allowed.store(false, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(400));
            }
            let _ = stream.write_all(response.as_bytes());
            String::from_utf8(bytes).unwrap()
        })
    }
    fn ok_response() -> String {
        let body = json!({"choices":[{"message":{"content":"Allowed result"},"finish_reason":"stop"}],"usage":{}}).to_string();
        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)
    }

    #[test]
    fn minimal_config_preserves_credential_precedence_aliases_and_expiry() {
        let _guard = crate::CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        struct Restore(Vec<(&'static str, Option<std::ffi::OsString>)>);
        impl Drop for Restore {
            fn drop(&mut self) {
                for (name, value) in &self.0 {
                    unsafe { match value { Some(value) => std::env::set_var(name, value), None => std::env::remove_var(name) } }
                }
            }
        }
        let names = ["OCTOS_CONFIG_DIR", "OPENAI_API_KEY", "MOONSHOT_API_KEY", "KIMI_API_KEY", "kimi_api_key", "ROBRIX_TRANSPORT_CUSTOM_KEY"];
        let _restore = Restore(names.into_iter().map(|name| (name, std::env::var_os(name))).collect());
        let directory = tempfile::tempdir().unwrap();
        unsafe {
            for name in names { std::env::remove_var(name); }
            std::env::set_var("OCTOS_CONFIG_DIR", directory.path());
            std::env::set_var("OPENAI_API_KEY", "process-openai");
            std::env::set_var("MOONSHOT_API_KEY", "process-moonshot");
        }
        let check = |provider: &str, value: Value, expected: Option<&str>| {
            let config: ModelConfig = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(config.api_key(provider).as_deref(), expected);
            #[cfg(feature = "embedded")]
            {
                let canonical: octos_cli::config::Config = serde_json::from_value(value).unwrap();
                assert_eq!(config.api_key(provider), canonical.get_api_key_with_env(provider, canonical.api_key_env.as_deref()).ok());
            }
        };
        let write_auth = |expiry: Value| std::fs::write(directory.path().join("auth.json"), json!({"credentials":{
            "openai":{"access_token":"stored-openai","expires_at":expiry,"provider":"openai","auth_method":"paste_token"},
            "moonshot":{"access_token":"stored-moonshot","provider":"moonshot","auth_method":"paste_token"}
        }}).to_string()).unwrap();
        write_auth(Value::Null);
        check("openai", json!({"env_vars":{"OPENAI_API_KEY":"config-openai"}}), Some("stored-openai"));
        check("openai", json!({"api_key_env":"OPENAI_API_KEY"}), Some("stored-openai"));
        check("moonshot", json!({"api_key_env":"KIMI_API_KEY"}), Some("stored-moonshot"));
        check("moonshot", json!({"api_key_env":"kimi_api_key"}), None);
        check("openai", json!({"api_key_env":"ROBRIX_TRANSPORT_CUSTOM_KEY"}), None);
        check("openai", json!({"api_key_env":"ROBRIX_TRANSPORT_CUSTOM_KEY","env_vars":{"ROBRIX_TRANSPORT_CUSTOM_KEY":"proxy-key"}}), Some("proxy-key"));
        write_auth(json!("2000-01-01T00:00:00Z"));
        check("openai", json!({"env_vars":{"OPENAI_API_KEY":"config-openai"}}), Some("config-openai"));
        check("openai", json!({"env_vars":{"OPENAI_API_KEY":""}}), Some("process-openai"));
        std::fs::remove_file(directory.path().join("auth.json")).unwrap();
        check("moonshot", json!({"api_key_env":"KIMI_API_KEY"}), Some("process-moonshot"));
        check("moonshot", json!({"api_key_env":"KIMI_API_KEY","env_vars":{"MOONSHOT_API_KEY":"profile-moonshot"}}), Some("profile-moonshot"));
        check("custom", json!({"api_key_env":"ROBRIX_TRANSPORT_CUSTOM_KEY","env_vars":{"ROBRIX_TRANSPORT_CUSTOM_KEY":""}}), None);
    }

    #[test]
    fn minimal_config_preserves_model_fields_and_keychain_encoding() {
        let value = json!({"provider":"custom","model":"selected-model","base_url":"https://service.example/v1",
            "api_type":"anthropic","api_key_env":"PRIVATE_KEY","env_vars":{"PRIVATE_KEY":"fixture"},
            "model_hints":{"uses_completion_tokens":true,"fixed_temperature":true},
            "mcp_servers":[],"sandbox":{},"unrelated_future_field":true});
        let config: ModelConfig = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(config.provider.as_deref(), Some("custom"));
        assert_eq!(config.model.as_deref(), Some("selected-model"));
        assert_eq!(config.base_url.as_deref(), Some("https://service.example/v1"));
        assert_eq!(config.api_type.as_deref(), Some("anthropic"));
        assert!(config.model_hints.as_ref().unwrap().uses_completion_tokens);
        #[cfg(feature = "embedded")]
        {
            let canonical: octos_cli::config::Config = serde_json::from_value(value).unwrap();
            assert_eq!(config.provider, canonical.provider);
            assert_eq!(config.model, canonical.model);
            assert_eq!(config.base_url, canonical.base_url);
            assert_eq!(config.api_type, canonical.api_type);
            assert_eq!(config.api_key_env, canonical.api_key_env);
            assert_eq!(config.env_vars, canonical.env_vars);
            assert_eq!(config.model_hints, canonical.model_hints);
        }
        assert_eq!(decode_keychain_hex("610a62"), Some("a\nb".into()));
        assert_eq!(decode_keychain_hex("41424344"), None);
        assert_eq!(decode_keychain_hex("deadbeef"), None);
    }

    #[test]
    fn identity_changes_for_endpoint_credential_model_and_protocol() {
        let fields = ["openai", "https://example.com/v1/chat/completions", "model-a", "openai", "secret-a"];
        let expected = identity(b"private-salt", &fields);
        for index in 0..fields.len() {
            let mut changed = fields;
            changed[index] = "different";
            assert_ne!(expected, identity(b"private-salt", &changed));
        }
        assert!(!expected.contains("secret-a"));
        assert_ne!(expected, identity(b"other-profile", &fields));
    }

    #[test]
    fn endpoints_require_https_or_literal_loopback_and_no_hidden_credentials() {
        for base in ["http://localhost:11434/v1", "http://127.0.0.1.evil/v1", "http://10.0.0.1/v1", "https://user:key@example.com/v1", "https://example.com/v1?key=secret", "https://example.com/v1#secret", "file:///secret"] {
            assert!(endpoint(base, Protocol::OpenAi).is_err(), "{base}");
        }
        assert!(endpoint("http://127.0.0.1:11434/v1", Protocol::OpenAi).unwrap().1);
        assert!(endpoint("http://[::1]:11434/v1", Protocol::OpenAi).unwrap().1);
        assert!(!endpoint("https://localhost/v1", Protocol::OpenAi).unwrap().1);
        assert_eq!(endpoint("https://api.anthropic.com/v1", Protocol::Anthropic).unwrap().0, "https://api.anthropic.com/v1/messages");
    }

    #[test]
    fn configured_recipient_tracks_real_key_endpoint_model_independently_of_agent_backend() {
        let _guard = crate::CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        struct Restore {
            config: Option<std::ffi::OsString>, command: Option<std::ffi::OsString>, provider: Option<String>,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe {
                    match &self.config { Some(value) => std::env::set_var("OCTOS_CONFIG_DIR", value), None => std::env::remove_var("OCTOS_CONFIG_DIR") }
                    match &self.command { Some(value) => std::env::set_var("ROBRIX_AGENT_CMD", value), None => std::env::remove_var("ROBRIX_AGENT_CMD") }
                }
                match &self.provider { Some(value) => crate::providers::set_session(value), None => crate::providers::clear_session() }
            }
        }
        let _restore = Restore { config: std::env::var_os("OCTOS_CONFIG_DIR"), command: std::env::var_os("ROBRIX_AGENT_CMD"), provider: crate::providers::session_provider() };
        let directory = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("OCTOS_CONFIG_DIR", directory.path()); std::env::remove_var("ROBRIX_AGENT_CMD"); }
        crate::providers::clear_session();
        let mut config = json!({"provider":"openai","model":"fixture-model","base_url":"https://first.example/v1",
            "api_key_env":"ROBRIX_TEST_MODEL_CREDENTIAL","env_vars":{"ROBRIX_TEST_MODEL_CREDENTIAL":"fixture-key-one"}});
        let write = |value: &Value| std::fs::write(directory.path().join("config.json"), value.to_string()).unwrap();
        write(&config);
        let prefs = AgentPrefs::default();
        let first = resolve(&prefs).unwrap();
        assert_eq!(first.key, "fixture-key-one");
        config["env_vars"]["ROBRIX_TEST_MODEL_CREDENTIAL"] = json!("fixture-key-two");
        write(&config);
        let second = resolve(&prefs).unwrap();
        assert_ne!(first.recipient.id, second.recipient.id);
        config["base_url"] = json!("https://second.example/v1"); write(&config);
        let third = resolve(&prefs).unwrap();
        assert_ne!(second.recipient.id, third.recipient.id);
        assert_eq!(third.recipient.endpoint, "https://second.example/v1/chat/completions");
        config["model"] = json!("other-model"); write(&config);
        assert_ne!(third.recipient.id, resolve(&prefs).unwrap().recipient.id);
        for unsupported in ["gemini", "scenario", "arbitrary-provider"] {
            config["provider"] = json!(unsupported); write(&config);
            assert!(resolve(&prefs).is_err());
        }
        config["provider"] = json!("custom");
        config["base_url"] = json!("http://127.0.0.1:11434/v1");
        config.as_object_mut().unwrap().remove("api_key_env");
        write(&config);
        let custom = resolve(&prefs).unwrap();
        assert!(custom.recipient.local);
        assert!(custom.key.is_empty(), "custom endpoints do not inherit an arbitrary provider credential");
        unsafe { std::env::set_var("ROBRIX_AGENT_CMD", "opaque-test-agent"); }
        assert_eq!(resolve(&prefs).unwrap().recipient, custom.recipient, "backend selection does not change the host-owned model recipient");
    }

    #[tokio::test]
    async fn denied_private_context_never_opens_a_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let config = fixture(&format!("http://{}/v1", listener.local_addr().unwrap()), Protocol::OpenAi);
        let provider = GuardedProvider::new(config, approval(Arc::new(AtomicBool::new(false)))).unwrap();
        assert!(provider.chat(&[message(MessageRole::User, "private room")], &[], &ChatConfig::default()).await.is_err());
        assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    }

    #[tokio::test]
    async fn changed_model_configuration_is_rejected_before_transport() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let config = fixture(&format!("http://{}/v1", listener.local_addr().unwrap()), Protocol::OpenAi);
        let mut provider = GuardedProvider::new(config, approval(Arc::new(AtomicBool::new(true)))).unwrap();
        provider.check_config = Some(Arc::new(|| Err("Model configuration changed.".into())));
        let error = provider.chat(&[message(MessageRole::User, "private input")], &[], &ChatConfig::default()).await.unwrap_err().to_string();
        assert!(error.contains("configuration changed"));
        assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    }

    #[tokio::test]
    async fn tool_feedback_and_retained_history_require_current_approval() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let allowed = Arc::new(AtomicBool::new(true));
        let server = request(listener, ok_response(), None);
        let provider = GuardedProvider::new(fixture(&format!("http://{address}/v1"), Protocol::OpenAi), approval(allowed.clone())).unwrap();
        assert_eq!(provider.chat(&[message(MessageRole::User, "first approved input")], &[], &ChatConfig::default()).await.unwrap().content.as_deref(), Some("Allowed result"));
        let received = server.join().unwrap();
        assert!(received.contains("first approved input"));
        allowed.store(false, Ordering::SeqCst);
        // The listener is gone: a failed-connect error would expose a missing
        // gate, so assert the policy error on this second model request.
        let error = provider.chat(&[message(MessageRole::User, "retained history"), message(MessageRole::Tool, "new private tool result")], &[], &ChatConfig::default()).await.unwrap_err().to_string();
        assert!(error.contains("policy denied"));
    }

    #[tokio::test]
    async fn real_source_policy_blocks_encoded_private_tool_feedback_before_transport() {
        use std::sync::Mutex;
        use information_flow::{Registry, Source, ReaderScope, SharingDuration};
        let root = tempfile::tempdir().unwrap();
        let context = ContextId::Agent { account: "alice".into(), room: "room-a".into() };
        let recipient = Recipient::ModelProvider("test-recipient".into());
        let mut registry = Registry::open(root.path()).unwrap();
        registry.register_context(&context).unwrap();
        let source = Source::Room { account: "alice".into(), room: "room-a".into() };
        registry.add_sources(&context, [source.clone()]).unwrap();
        registry.grant_sharing(source, recipient.clone(), ReaderScope::Context(context.clone()), SharingDuration::RobrixSession).unwrap();
        let registry = Arc::new(Mutex::new(registry));
        let approval: Approval = {
            let registry = registry.clone(); let context = context.clone();
            Arc::new(move || registry.lock().unwrap().ensure_allowed(&context, &recipient))
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = request(listener, ok_response(), None);
        let mut provider = GuardedProvider::new(fixture(&format!("http://{address}/v1"), Protocol::OpenAi), approval.clone()).unwrap();
        provider.on_response = Some({
            let registry = registry.clone(); let context = context.clone();
            Arc::new(move || registry.lock().unwrap().add_influences(&context, [Influence::Model("test-recipient".into())]))
        });
        provider.chat(&[message(MessageRole::User, "approved room input")], &[], &ChatConfig::default()).await.unwrap();
        server.join().unwrap();
        assert!(registry.lock().unwrap().influences(&context).unwrap().contains(&Influence::Model("test-recipient".into())));
        // A tool joins provenance before delivering its result, independently
        // of the resulting bytes or the model's choice to encode them.
        registry.lock().unwrap().add_sources(&context, [Source::Room { account: "alice".into(), room: "room-b".into() }]).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let provider = GuardedProvider::new(fixture(&format!("http://{}/v1",listener.local_addr().unwrap()), Protocol::OpenAi), approval).unwrap();
        assert!(provider.chat(&[message(MessageRole::User, "retained history"), message(MessageRole::Tool, "cHJpdmF0ZSByb29tIGI=")], &[], &ChatConfig::default()).await.is_err());
        assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    }

    #[tokio::test]
    async fn reopening_durable_context_does_not_revive_old_model_transport() {
        use std::sync::Mutex;
        let root = tempfile::tempdir().unwrap();
        let context = ContextId::Agent { account: "alice".into(), room: "room-a".into() };
        let mut registry = information_flow::Registry::open(root.path()).unwrap();
        registry.register_context(&context).unwrap();
        let epoch = registry.context_epoch(&context).unwrap();
        let registry = Arc::new(Mutex::new(registry));
        let approve: Approval = {
            let registry = registry.clone(); let context = context.clone();
            Arc::new(move || {
                let registry = registry.lock().unwrap();
                registry.ensure_context_epoch(&context, epoch)?;
                registry.ensure_allowed(&context, &Recipient::ModelProvider("test-recipient".into()))
            })
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let provider = GuardedProvider::new(fixture(&format!("http://{}/v1", listener.local_addr().unwrap()), Protocol::OpenAi), approve).unwrap();
        {
            let mut registry = registry.lock().unwrap();
            registry.remove_context(&context);
            registry.register_context(&context).unwrap();
            assert_ne!(registry.context_epoch(&context).unwrap(), epoch);
        }
        let error = provider.chat(&[message(MessageRole::User, "old queued input")], &[], &ChatConfig::default()).await.unwrap_err();
        assert!(!error.to_string().contains("transport failed"));
        assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    }

    #[tokio::test]
    async fn service_errors_record_influence_before_reaching_agent_retry_logic() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = request(listener, "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(), None);
        let recorded = Arc::new(AtomicBool::new(false));
        let mut provider = GuardedProvider::new(fixture(&format!("http://{address}/v1"), Protocol::OpenAi), approval(Arc::new(AtomicBool::new(true)))).unwrap();
        provider.on_response = Some({
            let recorded = recorded.clone();
            Arc::new(move || { recorded.store(true, Ordering::SeqCst); Ok(()) })
        });
        assert!(provider.chat(&[message(MessageRole::User, "input")], &[], &ChatConfig::default()).await.unwrap_err().to_string().contains("503"));
        assert!(recorded.load(Ordering::SeqCst));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn model_response_is_not_delivered_if_influence_cannot_be_recorded() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = request(listener, ok_response(), None);
        let mut provider = GuardedProvider::new(fixture(&format!("http://{address}/v1"), Protocol::OpenAi), approval(Arc::new(AtomicBool::new(true)))).unwrap();
        provider.on_response = Some(Arc::new(|| Err("influence persistence blocked".into())));
        assert!(provider.chat(&[message(MessageRole::User, "input")], &[], &ChatConfig::default()).await.unwrap_err().to_string().contains("influence persistence blocked"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn model_redirect_is_never_followed() {
        let first = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let base = format!("http://{}/v1", first.local_addr().unwrap());
        let server = request(first, format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{}/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", target.local_addr().unwrap()), None);
        let provider = GuardedProvider::new(fixture(&base, Protocol::OpenAi), approval(Arc::new(AtomicBool::new(true)))).unwrap();
        let error = provider.chat(&[message(MessageRole::User, "private input")], &[], &ChatConfig::default()).await.unwrap_err().to_string();
        assert!(error.contains("307"));
        server.join().unwrap();
        assert_eq!(target.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    }

    #[tokio::test]
    async fn live_revocation_cancels_waiting_request() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let allowed = Arc::new(AtomicBool::new(true));
        let server = request(listener, ok_response(), Some(allowed.clone()));
        let provider = GuardedProvider::new(fixture(&base, Protocol::OpenAi), approval(allowed)).unwrap();
        let error = provider.chat(&[message(MessageRole::User, "approved at send")], &[], &ChatConfig::default()).await.unwrap_err().to_string();
        assert!(error.contains("policy denied"));
        server.join().unwrap();
    }

    #[test]
    fn both_protocols_preserve_tool_result_linkage() {
        let mut assistant = message(MessageRole::Assistant, "");
        assistant.tool_calls = Some(vec![ToolCall { id: "call-1".into(), name: "room_read".into(), arguments: json!({"room":"private"}), metadata: None }]);
        let mut result = message(MessageRole::Tool, "private tool result");
        result.tool_call_id = Some("call-1".into());
        let messages = [message(MessageRole::System, "instructions"), message(MessageRole::User, "question"), assistant, result];
        let config = fixture("https://example.com/v1", Protocol::OpenAi);
        let openai = openai_body(&config, &messages, &[], &ChatConfig::default());
        assert_eq!(openai["messages"][3]["tool_call_id"], "call-1");
        assert_eq!(openai["messages"][3]["content"], "private tool result");
        let anthropic = anthropic_body(&config, &messages, &[], &ChatConfig::default());
        assert_eq!(anthropic["messages"][2]["content"][0]["tool_use_id"], "call-1");
        assert_eq!(anthropic["messages"][2]["content"][0]["content"], "private tool result");
    }
}
