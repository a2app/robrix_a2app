use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::time::Duration;

use a2app_core::information_flow::{self, ContextId, Influence, Recipient};
use octos_core::{Message, MessageRole, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolChoice, ToolSpec};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{AgentPrefs, ModelRecipient, stored_api_key};

#[derive(Clone, Copy, PartialEq)]
enum Protocol { OpenAi, Anthropic, Gemini }

// Explicit OpenAI-compatible Gemini configurations retain their existing API;
// the registry default uses the native API to preserve audio/video inputs.
const GEMINI_OPENAI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta/openai";

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
    #[cfg(target_os = "linux")]
    {
        // Upstream's store is independent of OCTOS_CONFIG_DIR and supports
        // both bare env-var accounts and scoped ENV::profile accounts.
        read_linux_secret(&dirs::home_dir()?.join(".octos").join("secrets"), account)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = account;
        None
    }
}

#[cfg(any(target_os = "linux", all(test, unix)))]
fn read_linux_secret(root: &std::path::Path, account: &str) -> Option<String> {
    use std::io::Read;
    // Keep account validation and byte-preserving UTF-8 decoding compatible
    // with octos_cli::auth::keychain::linux_file, without linking the CLI.
    if account.is_empty() || matches!(account, "." | "..") || account.contains(['/', '\\', '\0']) {
        return None;
    }
    let file = std::fs::File::open(root.join(account)).ok()?;
    if !file.metadata().ok()?.is_file() { return None; }
    const MAX_SECRET_BYTES: usize = 64 * 1024;
    let mut bytes = Vec::new();
    file.take((MAX_SECRET_BYTES + 1) as u64).read_to_end(&mut bytes).ok()?;
    if bytes.len() > MAX_SECRET_BYTES { return None; }
    String::from_utf8(bytes).ok()
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
        Protocol::Gemini => format!("{base_path}/generateContent"),
    };
    url.set_path(&path);
    Ok((url.to_string(), local))
}

pub(super) fn identity(salt: &[u8], fields: &[&str]) -> String {
    let mut digest = Sha256::new();
    digest.update(salt);
    for field in fields {
        digest.update((field.len() as u64).to_be_bytes());
        digest.update(field.as_bytes());
    }
    format!("model:{:x}", digest.finalize())
}

pub(super) fn identity_salt() -> Result<Vec<u8>, String> {
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

/// Image requests share the host's exact credential resolution. A custom base
/// belongs only to the provider selected in that config, never another key.
pub(super) fn image_service_config(provider: &str) -> Result<(String, String), String> {
    let config: ModelConfig = match crate::octos_config_candidates().into_iter().find(|path| path.exists()) {
        Some(path) => serde_json::from_str(&std::fs::read_to_string(path).map_err(|_| "Cannot read model configuration.")?)
            .map_err(|_| "Cannot parse model configuration.")?,
        None => ModelConfig::default(),
    };
    let selected = config.provider.as_deref() == Some(provider);
    let base = if selected { config.base_url.clone() } else { None }
        .or_else(|| octos_llm::registry::lookup(provider).and_then(|entry| entry.default_base_url).map(str::to_string))
        .ok_or("Configure an image provider API endpoint.")?;
    let credentials = if selected { config } else { ModelConfig { env_vars: config.env_vars, ..Default::default() } };
    let key = credentials.api_key(provider).ok_or("Image generation needs a configured OpenAI or Gemini API key. Open Setup AI Providers to add one.")?;
    Ok((base, key))
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
    if !matches!(name, "openai" | "anthropic" | "gemini" | "deepseek" | "moonshot" | "moonshot-coding" | "groq" | "openrouter" | "ollama" | "custom") {
        return Err("This model backend is not supported by the guarded transport. Select an OpenAI-compatible or Anthropic provider.".into());
    }
    let protocol = match config.api_type.as_deref() {
        Some("anthropic") => Protocol::Anthropic,
        Some("openai") => Protocol::OpenAi,
        Some("gemini") => Protocol::Gemini,
        Some(_) => return Err("Unsupported model API protocol. Choose openai, anthropic or gemini.".into()),
        None if name == "anthropic" => Protocol::Anthropic,
        None if name == "gemini" && config.base_url.as_deref().is_none_or(|base|
            entry.and_then(|entry| entry.default_base_url).is_some_and(|default| base.trim_end_matches('/') == default.trim_end_matches('/'))) => Protocol::Gemini,
        None => Protocol::OpenAi,
    };
    if name == "gemini" && protocol == Protocol::Anthropic && config.base_url.is_none() {
        return Err("Configure an explicit Anthropic-compatible API base URL for this model protocol.".into());
    }
    let registry_base = entry.and_then(|entry| entry.default_base_url);
    let base = config.base_url.as_deref().or(registry_base)
        .ok_or("Configure a model API base URL.")?;
    // A saved copy of Google's native default needs the same translation as
    // an omitted URL. Custom endpoints and explicit protocols remain intact.
    let base = if name == "gemini" && protocol == Protocol::OpenAi
        && registry_base.is_some_and(|default| base.trim_end_matches('/') == default.trim_end_matches('/')) {
        GEMINI_OPENAI_BASE_URL
    } else { base };
    // Only the built-in Ollama default is rewritten. Arbitrary HTTP DNS names
    // do not become trusted local services because of a user-supplied label.
    let base = if name == "ollama" && config.base_url.is_none() { "http://127.0.0.1:11434/v1" } else { base };
    let model = crate::prefs::Backend::Octos { provider: name.into() }.model_override(prefs)
        .or_else(|| config.model.clone()).or_else(|| entry.and_then(|entry| entry.default_model()).map(str::to_string))
        .ok_or("Choose a model for the configured provider.")?;
    let (mut endpoint, local) = endpoint(base, protocol)?;
    if protocol == Protocol::Gemini {
        let model_id = model.strip_prefix("models/").unwrap_or(&model);
        if model_id.is_empty() || model_id.contains(['/', '?', '#']) || model_id.contains(char::from(92)) || model_id.chars().any(char::is_control) {
            return Err("Invalid Gemini model identifier.".into());
        }
        let mut url = url::Url::parse(&endpoint).map_err(|_| "Invalid Gemini endpoint.")?;
        let prefix = url.path().trim_end_matches("/generateContent").to_string();
        url.set_path(&format!("{prefix}/models/{model_id}:generateContent"));
        endpoint = url.to_string();
    }
    let key = if entry.is_some_and(|entry| entry.requires_api_key) || config.api_key_env.is_some() { config.api_key(&selected).ok_or("Configure the API credential for this model provider.")? } else { String::new() };
    let protocol_name = match protocol { Protocol::OpenAi => "openai", Protocol::Anthropic => "anthropic", Protocol::Gemini => "gemini" };
    let id = identity(&identity_salt()?, &[name, &endpoint, &model, protocol_name, &key]);
    let hints = config.model_hints.unwrap_or_else(|| octos_llm::openai::ModelHints::detect(&model));
    Ok(Resolved {
        recipient: ModelRecipient { id, label: format!("{} / {}", name, model), endpoint, local },
        provider: name.into(), model, key, protocol, hints,
    })
}

pub(super) type Approval = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// The same activation, policy, cancellation, and recipient checks protect
/// both host-owned transports, including every subsequent tool-feedback turn.
#[derive(Clone)]
pub(super) struct Guard {
    pub(super) approve: Approval,
    pub(super) check_config: Approval,
    pub(super) on_response: Approval,
    pub(super) audit_context: Option<ContextId>,
}

impl Guard {
    pub(super) fn new(prefs: &AgentPrefs, context: ContextId, shutdown: Arc<AtomicBool>, model: &ModelRecipient) -> Result<Self, String> {
        let epoch = information_flow::context_epoch(&context)?;
        let prefs = prefs.clone();
        let expected = model.id.clone();
        let recipient = Recipient::ModelProvider(expected.clone());
        let check_config: Approval = Arc::new(move || {
            if super::current_recipient(&prefs)?.id != expected {
                return Err("Model configuration changed. Restart the agent and approve the current model service.".into());
            }
            Ok(())
        });
        let response_context = context.clone();
        let audit_context = context.clone();
        let response_model = model.id.clone();
        let approve: Approval = Arc::new(move || {
            if shutdown.load(Ordering::Acquire) { return Err("Model request cancelled.".into()); }
            information_flow::ensure_allowed_for_activation(&context, epoch, &recipient)?;
            Ok(())
        });
        Ok(Self { approve, check_config, audit_context: Some(audit_context), on_response: Arc::new(move || {
            information_flow::add_influences_for_activation(&response_context, epoch, [Influence::Model(response_model.clone())])
        }) })
    }

    pub(super) fn check(&self) -> Result<(), String> {
        (self.approve)()?;
        (self.check_config)()?;
        (self.approve)()
    }
}

pub(crate) fn provider(prefs: &AgentPrefs, context: ContextId, shutdown: Arc<AtomicBool>, outputs: Arc<crate::tool_output::ToolOutputStore>, preview: super::PreviewSink) -> Result<Arc<dyn LlmProvider>, String> {
    let config = resolve(prefs)?;
    let guard = Guard::new(prefs, context, shutdown, &config.recipient)?;
    let mut provider = GuardedProvider::new(config, guard.approve)?;
    provider.outputs = outputs;
    provider.audit_context = guard.audit_context;
    provider.check_config = Some(guard.check_config);
    provider.on_response = Some(guard.on_response);
    provider.preview = preview;
    Ok(Arc::new(provider))
}

struct GuardedProvider { config: Resolved, client: reqwest::Client, approve: Approval, check_config: Option<Approval>, on_response: Option<Approval>, audit_context: Option<ContextId>, outputs: Arc<crate::tool_output::ToolOutputStore>, preview: super::PreviewSink,
    /// The service answered a streamed request only unstreamed, so later calls skip streaming.
    is_stream_refused: AtomicBool }

impl GuardedProvider {
    fn new(config: Resolved, approve: Approval) -> Result<Self, String> {
        let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none())
            .no_proxy().referer(false).retry(reqwest::retry::never()).http1_only()
            .no_gzip().no_brotli().no_deflate().no_zstd()
            .timeout(Duration::from_secs(180)).build()
            .map_err(|_| "Cannot initialize the guarded model transport.")?;
        Ok(Self { config, client, approve, check_config: None, on_response: None, audit_context: None, outputs: Arc::default(), preview: Default::default(),
            is_stream_refused: AtomicBool::new(false) })
    }

    fn request(&self, url: &str, body: &Value) -> reqwest::RequestBuilder {
        let mut request = self.client.post(url).json(body);
        if !self.config.key.is_empty() {
            request = match self.config.protocol {
                Protocol::OpenAi => request.bearer_auth(&self.config.key),
                Protocol::Anthropic => request.header("x-api-key", &self.config.key),
                Protocol::Gemini => request.header("x-goog-api-key", &self.config.key),
            };
        }
        if self.config.protocol == Protocol::Anthropic { request = request.header("anthropic-version", "2023-06-01"); }
        request
    }

    /// Like `send`, but asks for a streamed reply and shows its text as it arrives.
    /// The reply is rebuilt into the body `send` returns, so parsing stays the same.
    /// `None` means the service refused to stream, so it sent no reply.
    async fn send_streaming(&self, body: &Value, show: &(dyn Fn(bool, &str) + Send + Sync)) -> eyre::Result<Option<Value>> {
        (self.approve)().map_err(eyre::Report::msg)?;
        if let Some(check) = &self.check_config { check().map_err(eyre::Report::msg)?; }
        (self.approve)().map_err(eyre::Report::msg)?;
        let mut body = body.clone();
        let url = match self.config.protocol {
            Protocol::OpenAi => {
                body["stream"] = json!(true);
                body["stream_options"] = json!({"include_usage": true});
                self.config.recipient.endpoint.clone()
            }
            Protocol::Anthropic => {
                body["stream"] = json!(true);
                // These models think anyway, but stream it as empty text unless asked for a summary.
                if is_claude_thinking_by_default(&self.config.model) { body["thinking"] = json!({"type":"adaptive","display":"summarized"}); }
                self.config.recipient.endpoint.clone()
            }
            Protocol::Gemini => {
                // Thinking models return readable summaries of their thinking only when asked.
                if is_gemini_thinking_model(&self.config.model) { body["generationConfig"]["thinkingConfig"]["includeThoughts"] = json!(true); }
                gemini_stream_endpoint(&self.config.recipient.endpoint)?
            }
        };
        let request = self.request(&url, &body);
        let response = async {
            let attempt = self.audit_context.as_ref().map(|context| a2app_core::protection_audit::Attempt::start(context,
                Some(Recipient::ModelProvider(self.config.recipient.id.clone())), a2app_core::protection_audit::ActivityKind::ModelRequest));
            let result = async {
                let mut response = request.send().await.map_err(|_| eyre::eyre!("Model transport failed."))?;
                let status = response.status().as_u16();
                // Some services take a request only unstreamed, or lack Gemini's stream method.
                if matches!(status, 400 | 404 | 405 | 415 | 422 | 501) { return Ok(None); }
                if !response.status().is_success() {
                    return Err(eyre::eyre!("Model service returned HTTP {status}. Redirects are not followed."));
                }
                // A service that ignores the stream request answers with one JSON body.
                let is_stream = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value.trim_start().to_ascii_lowercase().starts_with("text/event-stream"));
                let limit = if is_stream { MAX_STREAM_BYTES } else { MAX_REPLY_BYTES };
                let mut reply = StreamedReply::new(self.config.protocol, &self.config.provider);
                let mut bytes = Vec::new();
                let mut total = 0usize;
                let mut is_recorded = false;
                while let Some(chunk) = response.chunk().await.map_err(|_| eyre::eyre!("Model response could not be read."))? {
                    total += chunk.len();
                    if total > limit { return Err(eyre::eyre!("Model response exceeded the size limit.")); }
                    if !is_stream { bytes.extend_from_slice(&chunk); continue; }
                    for (is_thought, text) in reply.feed(&chunk)? {
                        // Output is shown only after its influence is recorded, and only while still approved.
                        if !is_recorded {
                            if let Some(record) = &self.on_response { record().map_err(eyre::Report::msg)?; }
                            is_recorded = true;
                        }
                        (self.approve)().map_err(eyre::Report::msg)?;
                        show(is_thought, &text);
                    }
                    // A service may hold the connection open after its last event.
                    if reply.is_closed { break; }
                }
                if is_stream { reply.finish().map(Some) } else {
                    serde_json::from_slice(&bytes).map(Some).map_err(|_| invalid_response())
                }
            }.await;
            if let Some(attempt) = attempt { attempt.finish(matches!(result, Ok(Some(_)))); }
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

    async fn send(&self, body: Value) -> eyre::Result<Value> {
        (self.approve)().map_err(eyre::Report::msg)?;
        if let Some(check) = &self.check_config { check().map_err(eyre::Report::msg)?; }
        (self.approve)().map_err(eyre::Report::msg)?;
        let request = self.request(&self.config.recipient.endpoint, &body);
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
                    if bytes.len() + chunk.len() > MAX_REPLY_BYTES { return Err(eyre::eyre!("Model response exceeded the size limit.")); }
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
        let turns = super::multimodal::prepare(messages, &self.outputs, self.audit_context.as_ref()).map_err(eyre::Report::msg)?;
        let plain: Vec<Message> = turns.iter().map(|turn| turn.message.clone()).collect();
        let body = match self.config.protocol {
            Protocol::OpenAi => {
                let mut body = openai_body(&self.config, &plain, tools, config);
                for (value, turn) in body["messages"].as_array_mut().unwrap().iter_mut().zip(&turns) {
                    if !turn.attachments.is_empty() {
                        let mut parts = vec![json!({"type":"text","text":turn.message.content})];
                        parts.extend(super::multimodal::parts(&turn.attachments, super::multimodal::Format::OpenAi, &self.config.provider));
                        value["content"] = json!(parts);
                    }
                }
                body
            }
            Protocol::Anthropic => anthropic_multimodal_body(&self.config, &turns, tools, config),
            Protocol::Gemini => super::gemini::body(&turns, tools, config),
        };
        let result = match self.preview.show().filter(|_| !self.is_stream_refused.load(Ordering::Acquire)) {
            Some(show) => match self.send_streaming(&body, &*show).await {
                Ok(Some(value)) => Ok(value),
                // The refusal is service output too, so its influence is recorded before asking again.
                Ok(None) => {
                    if let Some(record) = &self.on_response { record().map_err(eyre::Report::msg)?; }
                    let result = self.send(body).await;
                    if result.is_ok() { self.is_stream_refused.store(true, Ordering::Release); }
                    result
                }
                Err(error) => Err(error),
            },
            None => self.send(body).await,
        };
        // Service failures can influence retries too. Persist the influence
        // before returning either an error, text, or proposed tool calls.
        if let Some(record) = &self.on_response { record().map_err(eyre::Report::msg)?; }
        let value = result?;
        match self.config.protocol {
            Protocol::OpenAi => parse_openai(value, &self.config.provider),
            Protocol::Anthropic => parse_anthropic(value),
            Protocol::Gemini => super::gemini::parse(value),
        }
    }
    /// Replays the one guarded completion the way the confined worker does, keeping
    /// the reasoning and tool metadata that the default stream drops.
    async fn chat_stream(&self, messages: &[Message], tools: &[ToolSpec], config: &ChatConfig) -> eyre::Result<octos_llm::ChatStream> {
        Ok(octos_llm::host::response_stream(self.chat(messages, tools, config).await?))
    }
    fn model_id(&self) -> &str { &self.config.model }
    fn provider_name(&self) -> &str { &self.config.provider }
}

/// Gemini streams from its own method on the same model, with SSE framing.
fn gemini_stream_endpoint(endpoint: &str) -> eyre::Result<String> {
    let mut url = url::Url::parse(endpoint).map_err(|_| eyre::eyre!("Invalid Gemini endpoint."))?;
    let model = url.path().strip_suffix(":generateContent").ok_or_else(|| eyre::eyre!("Invalid Gemini endpoint."))?.to_string();
    url.set_path(&format!("{model}:streamGenerateContent"));
    url.set_query(Some("alt=sse"));
    Ok(url.to_string())
}

/// Claude 5 models think by default, so asking for a summary changes only what they show.
fn is_claude_thinking_by_default(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    let Some((family, version)) = model.strip_prefix("claude-").and_then(|rest| rest.split_once('-')) else { return false };
    let major = version.split('-').next().and_then(|major| major.parse::<u32>().ok());
    match family {
        "fable" | "mythos" => true,
        "opus" | "sonnet" | "haiku" => major.is_some_and(|major| major >= 5),
        _ => false,
    }
}

/// Gemini 2.5 and later can summarize their thinking; older models may reject the request.
fn is_gemini_thinking_model(model: &str) -> bool {
    let model = model.strip_prefix("models/").unwrap_or(model).to_ascii_lowercase();
    let Some(version) = model.strip_prefix("gemini-") else { return false };
    let mut numbers = version.split(|c: char| !c.is_ascii_digit()).map(|number| number.parse::<u32>().ok());
    let (major, minor) = (numbers.next().flatten(), numbers.next().flatten().unwrap_or(0));
    major.is_some_and(|major| (major, minor) >= (2, 5))
}

/// Groq, OpenRouter and Ollama name the reasoning field `reasoning`, and take none back.
fn reasoning_field(provider: &str) -> &'static str {
    if matches!(provider, "groq" | "openrouter" | "ollama") { "reasoning" } else { "reasoning_content" }
}

/// A reply's reasoning, from `reasoning_content` or the service's own `field`.
fn reasoning_of<'a>(message: &'a Value, field: &str) -> Option<&'a str> {
    ["reasoning_content", field].into_iter().find_map(|field| message[field].as_str().filter(|text| !text.is_empty()))
}

/// The most tool calls or content blocks one streamed reply may index.
const MAX_STREAMED_CALLS: usize = 256;
/// The most a model reply may hold, streamed or not.
const MAX_REPLY_BYTES: usize = 16 * 1024 * 1024;
/// SSE framing can outweigh small deltas many times over, so it has its own bound.
const MAX_STREAM_BYTES: usize = 4 * MAX_REPLY_BYTES;

fn invalid_response() -> eyre::Report { eyre::eyre!("Model service returned an invalid response.") }

/// Adds text to what one read shows, joined to a last piece of the same kind.
fn push_shown(shown: &mut Vec<(bool, String)>, is_thought: bool, text: &str) {
    if text.is_empty() { return; }
    match shown.last_mut() {
        Some((kind, last)) if *kind == is_thought => last.push_str(text),
        _ => shown.push((is_thought, text.to_string())),
    }
}

/// Rebuilds a streamed (SSE) reply into the JSON body a non-streamed request gets.
struct StreamedReply {
    protocol: Protocol,
    /// The OpenAI-style field this service streams reasoning in.
    reasoning_field: &'static str,
    /// Sorts `<think>` spans out of shown text, as the agent loop does with the whole reply.
    think: octos_llm::ThinkTagStreamSplitter,
    /// The start of a line that has not fully arrived.
    buffer: Vec<u8>,
    /// The data lines of the event being read.
    data: String,
    /// A completion marker arrived, so the reply is whole.
    is_done: bool,
    /// The terminal marker arrived, so nothing more needs reading.
    is_closed: bool,
    text: String,
    reasoning: String,
    /// OpenAI tool calls, Anthropic content blocks or Gemini parts, in stream order,
    /// each with its streamed text: call arguments, block text or tool input JSON.
    items: Vec<(Value, String)>,
    finish: Value,
    usage: Value,
    has_candidate: bool,
    /// Gemini's feedback on a prompt it blocked.
    blocked: Option<Value>,
    /// The bytes the rebuilt reply holds so far.
    size: usize,
}

impl StreamedReply {
    fn new(protocol: Protocol, provider: &str) -> Self {
        Self { protocol, reasoning_field: reasoning_field(provider), think: Default::default(), buffer: Vec::new(), data: String::new(),
            is_done: false, is_closed: false, text: String::new(), reasoning: String::new(), items: Vec::new(), finish: Value::Null,
            usage: json!({}), has_candidate: false, blocked: None, size: 0 }
    }

    /// Shows reply text the way the agent loop will sort it.
    fn show_text(&mut self, shown: &mut Vec<(bool, String)>, text: &str) {
        // The splitter's time grows with the square of one call's text, so it gets short slices.
        let (mut content, mut thinking, mut rest) = (String::new(), String::new(), text);
        while !rest.is_empty() {
            let (slice, tail) = rest.split_at(rest.floor_char_boundary(4096));
            let (text, thought) = self.think.feed(slice);
            content.push_str(&text);
            thinking.push_str(&thought);
            rest = tail;
        }
        push_shown(shown, true, &thinking);
        push_shown(shown, false, &content);
    }

    /// Takes the stream's next bytes and returns the text they add, as `(is_thought, text)`.
    fn feed(&mut self, bytes: &[u8]) -> eyre::Result<Vec<(bool, String)>> {
        // Only new bytes are searched, so floods of empty events or comments stay cheap.
        let mut scanned = self.buffer.len();
        self.buffer.extend_from_slice(bytes);
        let mut start = 0;
        let mut shown = Vec::new();
        while let Some(end) = self.buffer[scanned..].iter().position(|&byte| byte == b'\n').map(|at| scanned + at) {
            let line = &self.buffer[start..end];
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            start = end + 1;
            scanned = start;
            if line.is_empty() {
                // A blank line ends the event.
                if !self.data.is_empty() {
                    let data = std::mem::take(&mut self.data);
                    self.event(&data, &mut shown)?;
                    self.data = data;
                    self.data.clear();
                }
            } else if let Some(value) = line.strip_prefix(b"data:") {
                let value = std::str::from_utf8(value.strip_prefix(b" ").unwrap_or(value)).map_err(|_| invalid_response())?;
                if !self.data.is_empty() { self.data.push('\n'); }
                self.data.push_str(value);
            }
        }
        self.buffer.drain(..start);
        if self.size + self.data.len() + self.buffer.len() > MAX_REPLY_BYTES {
            return Err(eyre::eyre!("Model response exceeded the size limit."));
        }
        Ok(shown)
    }

    fn event(&mut self, data: &str, shown: &mut Vec<(bool, String)>) -> eyre::Result<()> {
        if data == "[DONE]" {
            self.is_done = true;
            self.is_closed = true;
            return Ok(());
        }
        let value: Value = serde_json::from_str(data).map_err(|_| invalid_response())?;
        if value.get("error").is_some_and(|error| !error.is_null()) || value["type"] == "error" {
            return Err(eyre::eyre!("Model service stopped the reply with an error."));
        }
        match self.protocol {
            Protocol::OpenAi => self.openai(&value, shown),
            Protocol::Anthropic => self.anthropic(&value, shown),
            Protocol::Gemini => { self.gemini(&value, shown); Ok(()) }
        }
    }

    fn openai(&mut self, value: &Value, shown: &mut Vec<(bool, String)>) -> eyre::Result<()> {
        // Kimi streams usage inside the last choice, and Groq under `x_groq`.
        let usage = [&value["usage"], &value["choices"][0]["usage"], &value["x_groq"]["usage"]];
        if let Some(usage) = usage.into_iter().find(|usage| usage.is_object()) { self.usage = usage.clone(); }
        let Some(choice) = value["choices"].get(0) else { return Ok(()) };
        let delta = &choice["delta"];
        if let Some(text) = reasoning_of(delta, self.reasoning_field) {
            self.reasoning.push_str(text);
            self.size += text.len();
            push_shown(shown, true, text);
        }
        if let Some(text) = delta["content"].as_str().filter(|text| !text.is_empty()) {
            self.text.push_str(text);
            self.size += text.len();
            self.show_text(shown, text);
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let index = call["index"].as_u64().unwrap_or(0) as usize;
            if index >= MAX_STREAMED_CALLS { return Err(eyre::eyre!("Model service returned too many tool calls.")); }
            while self.items.len() <= index { self.items.push((json!({"type":"function","function":{}}), String::new())); }
            let (item, arguments) = &mut self.items[index];
            if let Some(id) = call["id"].as_str() { item["id"] = json!(id); self.size += id.len(); }
            if let Some(name) = call["function"]["name"].as_str() { item["function"]["name"] = json!(name); self.size += name.len(); }
            if let Some(fragment) = call["function"]["arguments"].as_str() { arguments.push_str(fragment); self.size += fragment.len(); }
            if let Some(extra) = call.get("extra_content").filter(|extra| !extra.is_null()) {
                item["extra_content"] = extra.clone();
                self.size += extra.to_string().len();
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish = json!(reason);
            self.is_done = true;
        }
        Ok(())
    }

    fn anthropic(&mut self, value: &Value, shown: &mut Vec<(bool, String)>) -> eyre::Result<()> {
        let index = || match value["index"].as_u64() {
            Some(index) if (index as usize) < MAX_STREAMED_CALLS => Ok(index as usize),
            _ => Err(invalid_response()),
        };
        match value["type"].as_str() {
            Some("message_start") => if let Some(usage) = value["message"].get("usage").filter(|usage| usage.is_object()) { self.usage = usage.clone(); },
            Some("content_block_start") => {
                let index = index()?;
                let block = &value["content_block"];
                // A block starts once, so text already shown is never replaced.
                if !block.is_object() || self.items.get(index).is_some_and(|(started, _)| !started.is_null()) { return Err(invalid_response()); }
                while self.items.len() <= index { self.items.push((Value::Null, String::new())); }
                // Only text and thinking blocks keep their text, so only theirs is shown.
                let (text, is_thought) = match block["type"].as_str() {
                    Some("text") => (block["text"].as_str().unwrap_or_default(), false),
                    Some("thinking") => (block["thinking"].as_str().unwrap_or_default(), true),
                    _ => ("", false),
                };
                self.size += block.to_string().len();
                self.items[index] = (block.clone(), text.to_string());
                if is_thought { push_shown(shown, true, text); } else { self.show_text(shown, text); }
            }
            Some("content_block_delta") => {
                let index = index()?;
                let (block, body) = self.items.get_mut(index).filter(|(block, _)| block.is_object()).ok_or_else(invalid_response)?;
                let delta = &value["delta"];
                // Whether the added text is thinking, or `None` for tool input, which is not shown.
                let (added, is_thought) = match (delta["type"].as_str(), block["type"].as_str()) {
                    (Some("text_delta"), Some("text")) => (delta["text"].as_str().unwrap_or_default(), Some(false)),
                    (Some("thinking_delta"), Some("thinking")) => (delta["thinking"].as_str().unwrap_or_default(), Some(true)),
                    // The reply keeps no text from other blocks, so none may be shown.
                    (Some("text_delta" | "thinking_delta"), _) => return Err(invalid_response()),
                    (Some("input_json_delta"), Some("tool_use")) => (delta["partial_json"].as_str().unwrap_or_default(), None),
                    _ => return Ok(()),
                };
                body.push_str(added);
                self.size += added.len();
                match is_thought {
                    Some(true) => push_shown(shown, true, added),
                    Some(false) => self.show_text(shown, added),
                    None => {}
                }
            }
            Some("message_delta") => {
                if let Some(reason) = value["delta"]["stop_reason"].as_str() { self.finish = json!(reason); }
                for (key, count) in value["usage"].as_object().into_iter().flatten() { self.usage[key] = count.clone(); }
            }
            Some("message_stop") => {
                self.is_done = true;
                self.is_closed = true;
            }
            _ => {}
        }
        Ok(())
    }

    fn gemini(&mut self, value: &Value, shown: &mut Vec<(bool, String)>) {
        if value["usageMetadata"].is_object() { self.usage = value["usageMetadata"].clone(); }
        if value["promptFeedback"]["blockReason"].is_string() { self.blocked = Some(value["promptFeedback"].clone()); }
        let Some(candidate) = value["candidates"].get(0) else { return };
        self.has_candidate = true;
        for part in candidate["content"]["parts"].as_array().into_iter().flatten() {
            if let Some(text) = part["text"].as_str() {
                if part["thought"].as_bool() == Some(true) { push_shown(shown, true, text); } else { self.show_text(shown, text); }
            }
            self.size += part.to_string().len();
            self.items.push((part.clone(), String::new()));
        }
        if let Some(reason) = candidate["finishReason"].as_str() {
            self.finish = json!(reason);
            self.is_done = true;
        }
    }

    fn finish(self) -> eyre::Result<Value> {
        if !self.has_candidate && let Some(feedback) = self.blocked {
            // Parsed like a blocked non-streamed reply.
            return Ok(json!({"candidates": [], "promptFeedback": feedback, "usageMetadata": self.usage}));
        }
        if !self.is_done { return Err(eyre::eyre!("The model's reply ended early.")); }
        Ok(match self.protocol {
            Protocol::OpenAi => {
                let mut message = json!({"content": (!self.text.is_empty()).then_some(self.text)});
                if !self.reasoning.is_empty() { message["reasoning_content"] = json!(self.reasoning); }
                if !self.items.is_empty() {
                    message["tool_calls"] = self.items.into_iter().map(|(mut call, arguments)| {
                        call["function"]["arguments"] = json!(arguments);
                        call
                    }).collect();
                }
                json!({"choices":[{"message": message, "finish_reason": self.finish}], "usage": self.usage})
            }
            Protocol::Anthropic => {
                let mut content = Vec::with_capacity(self.items.len());
                for (mut block, body) in self.items {
                    if block["type"] == "text" { block["text"] = json!(body); }
                    if block["type"] == "thinking" { block["thinking"] = json!(body); }
                    if block["type"] == "tool_use" && !body.trim().is_empty() {
                        block["input"] = serde_json::from_str(&body).map_err(|_| eyre::eyre!("Malformed model tool arguments."))?;
                    }
                    content.push(block);
                }
                json!({"content": content, "stop_reason": self.finish, "usage": self.usage})
            }
            Protocol::Gemini => {
                let parts: Vec<Value> = self.items.into_iter().map(|(part, _)| part).collect();
                json!({"candidates":[{"content":{"parts": parts}, "finishReason": self.finish}], "usageMetadata": self.usage})
            }
        })
    }
}

fn openai_body(resolved: &Resolved, messages: &[Message], tools: &[ToolSpec], config: &ChatConfig) -> Value {
    let messages: Vec<_> = messages.iter().map(|message| {
        let mut value = json!({"role": message.role.as_str(), "content": message.content});
        if let Some(id) = &message.tool_call_id { value["tool_call_id"] = json!(id); }
        // Services that name it `reasoning` take none back; Groq rejects unknown fields.
        if let Some(reasoning) = message.reasoning_content.as_ref().filter(|_| reasoning_field(&resolved.provider) == "reasoning_content") {
            value["reasoning_content"] = json!(reasoning);
        }
        if let Some(calls) = &message.tool_calls {
            value["tool_calls"] = json!(calls.iter().map(|call| {
                let mut value = json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments.to_string()}});
                // Gemini requires the original signature on subsequent tool
                // turns. Only replay that known field, never arbitrary metadata.
                if resolved.provider == "gemini" {
                    if let Some(signature) = call.metadata.as_ref().and_then(|metadata| metadata["thought_signature"].as_str()).filter(|signature| !signature.is_empty()) {
                        value["extra_content"] = json!({"google":{"thought_signature":signature}});
                    }
                }
                value
            }).collect::<Vec<_>>());
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

#[cfg(test)]
fn anthropic_body(resolved: &Resolved, messages: &[Message], tools: &[ToolSpec], config: &ChatConfig) -> Value {
    let turns = messages.iter().cloned().map(|message| super::multimodal::Turn { message, attachments: Vec::new() }).collect::<Vec<_>>();
    anthropic_multimodal_body(resolved, &turns, tools, config)
}

fn anthropic_multimodal_body(resolved: &Resolved, input: &[super::multimodal::Turn], tools: &[ToolSpec], config: &ChatConfig) -> Value {
    let mut system = Vec::new();
    let mut turns: Vec<Value> = Vec::new();
    for turn in input {
        let message = &turn.message;
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
        content.extend(super::multimodal::parts(&turn.attachments, super::multimodal::Format::Anthropic, &resolved.provider));
        if let Some(previous) = turns.last_mut().filter(|previous| previous["role"] == role) {
            previous["content"].as_array_mut().unwrap().extend(content);
        } else { turns.push(json!({"role":role,"content":content})); }
    }
    let mut value = json!({"model":resolved.model,"system":system.join("\n\n"),"messages":turns,"max_tokens":config.max_tokens.unwrap_or(8192),"stream":false});
    // Like Octos: Claude 4.7 and later reject sampling settings, and 0.0 is the agent's unset default.
    if let Some(temperature) = config.temperature.filter(|temperature| *temperature != 0.0 && is_sampling_accepted(&resolved.model)) {
        value["temperature"] = json!(temperature);
    }
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

/// Octos sends sampling settings over the Anthropic API only to GLM models.
fn is_sampling_accepted(model: &str) -> bool {
    model.rsplit('/').next().unwrap_or(model).trim().to_ascii_lowercase().starts_with("glm-")
}

fn number(value: &Value) -> u32 { value.as_u64().unwrap_or(0).min(u32::MAX as u64) as u32 }
fn required(value: &Value) -> eyre::Result<String> {
    value.as_str().map(str::to_string).ok_or_else(|| eyre::eyre!("Malformed model response."))
}
fn parse_openai(value: Value, provider: &str) -> eyre::Result<ChatResponse> {
    let choice = value["choices"].as_array().and_then(|choices| choices.first()).ok_or_else(|| eyre::eyre!("Missing model response."))?;
    let message = &choice["message"];
    let mut calls = Vec::new();
    for call in message["tool_calls"].as_array().into_iter().flatten() {
        let metadata = if provider == "gemini" {
            call["extra_content"]["google"]["thought_signature"].as_str().filter(|signature| !signature.is_empty())
                .map(|signature| json!({"thought_signature":signature}))
        } else { None };
        calls.push(ToolCall { id: required(&call["id"])?, name: required(&call["function"]["name"])?,
            arguments: serde_json::from_str(&required(&call["function"]["arguments"])?).map_err(|_| eyre::eyre!("Malformed model tool arguments."))?, metadata });
    }
    let usage = &value["usage"];
    let cached = number(&usage["prompt_tokens_details"]["cached_tokens"]);
    Ok(ChatResponse {
        content: message["content"].as_str().map(str::to_string), reasoning_content: reasoning_of(message, reasoning_field(provider)).map(str::to_string),
        stop_reason: match choice["finish_reason"].as_str() { Some("tool_calls") => StopReason::ToolUse, Some("length") => StopReason::MaxTokens, Some("content_filter") => StopReason::ContentFiltered, _ => StopReason::EndTurn },
        tool_calls: calls, usage: TokenUsage { input_tokens: number(&usage["prompt_tokens"]).saturating_sub(cached), output_tokens: number(&usage["completion_tokens"]), cache_read_tokens: cached, ..Default::default() }, provider_index: None,
    })
}
fn parse_anthropic(value: Value) -> eyre::Result<ChatResponse> {
    let blocks = value["content"].as_array().ok_or_else(|| eyre::eyre!("Missing model response."))?;
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => text.push_str(&required(&block["text"])?),
            Some("thinking") => reasoning.push_str(block["thinking"].as_str().unwrap_or_default()),
            Some("tool_use") => calls.push(ToolCall { id: required(&block["id"])?, name: required(&block["name"])?, arguments: block["input"].clone(), metadata: None }),
            _ => {}
        }
    }
    let usage = &value["usage"];
    Ok(ChatResponse {
        content: (!text.is_empty()).then_some(text), reasoning_content: (!reasoning.is_empty()).then_some(reasoning), tool_calls: calls,
        stop_reason: match value["stop_reason"].as_str() { Some("tool_use") => StopReason::ToolUse, Some("max_tokens") => StopReason::MaxTokens, Some("stop_sequence") => StopReason::StopSequence, _ => StopReason::EndTurn },
        usage: TokenUsage { input_tokens: number(&usage["input_tokens"]), output_tokens: number(&usage["output_tokens"]), cache_read_tokens: number(&usage["cache_read_input_tokens"]), cache_write_tokens: number(&usage["cache_creation_input_tokens"]), ..Default::default() }, provider_index: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    pub(super) fn fixture(base: &str, protocol: Protocol) -> Resolved {
        let (endpoint, local) = endpoint(base, protocol).unwrap();
        Resolved {
            recipient: ModelRecipient { id: "test-recipient".into(), label: "Test model".into(), endpoint, local },
            provider: "openai".into(), model: "test-model".into(), key: "fixture-credential".into(),
            protocol, hints: octos_llm::openai::ModelHints::default(),
        }
    }
    pub(super) fn message(role: MessageRole, content: &str) -> Message {
        Message { role, content: content.into(), media: vec![], tool_calls: None, tool_call_id: None,
            reasoning_content: None, client_message_id: None, thread_id: None, timestamp: chrono::Utc::now() }
    }
    pub(super) fn approval(allowed: Arc<AtomicBool>) -> Approval {
        Arc::new(move || if allowed.load(Ordering::SeqCst) { Ok(()) } else { Err("Private source policy denied this recipient.".into()) })
    }
    /// Reads one request, with its whole body, from a test connection.
    pub(super) fn read_request(stream: &mut std::net::TcpStream) -> String {
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
        String::from_utf8(bytes).unwrap()
    }
    pub(super) fn request(listener: TcpListener, response: String, revoke: Option<Arc<AtomicBool>>) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let received = read_request(&mut stream);
            if let Some(allowed) = revoke {
                allowed.store(false, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(400));
            }
            let _ = stream.write_all(response.as_bytes());
            received
        })
    }
    pub(super) fn ok_response() -> String {
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

    #[cfg(unix)]
    #[test]
    fn linux_secret_store_reads_scoped_accounts_and_rejects_invalid_files() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("secrets");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("OPENAI_API_KEY"), "plain-key").unwrap();
        std::fs::write(root.join("PRIVATE_KEY::profile-id"), "first\nsecond\n").unwrap();
        assert_eq!(read_linux_secret(&root, "OPENAI_API_KEY").as_deref(), Some("plain-key"));
        assert_eq!(read_linux_secret(&root, "PRIVATE_KEY::profile-id").as_deref(), Some("first\nsecond\n"));
        for account in ["", ".", "..", "../OPENAI_API_KEY", "nested/key", "nested\\key", "bad\0key", "missing"] {
            assert!(read_linux_secret(&root, account).is_none());
        }
        std::fs::write(root.join("invalid-utf8"), [0xff]).unwrap();
        std::fs::write(root.join("oversized"), vec![b'x'; 64 * 1024 + 1]).unwrap();
        std::fs::create_dir(root.join("directory")).unwrap();
        for account in ["invalid-utf8", "oversized", "directory"] {
            assert!(read_linux_secret(&root, account).is_none());
        }
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
        for unsupported in ["scenario", "arbitrary-provider"] {
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

    #[test]
    fn gemini_resolution_uses_native_media_default_and_preserves_explicit_endpoints() {
        let _guard = crate::CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        struct Restore { config: Option<std::ffi::OsString>, provider: Option<String> }
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe { match &self.config {
                    Some(value) => std::env::set_var("OCTOS_CONFIG_DIR", value),
                    None => std::env::remove_var("OCTOS_CONFIG_DIR"),
                } }
                match &self.provider { Some(value) => crate::providers::set_session(value), None => crate::providers::clear_session() }
            }
        }
        let _restore = Restore { config: std::env::var_os("OCTOS_CONFIG_DIR"), provider: crate::providers::session_provider() };
        let directory = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("OCTOS_CONFIG_DIR", directory.path()); }
        crate::providers::clear_session();
        let mut config = json!({"provider":"gemini","env_vars":{"GEMINI_API_KEY":"AQ.synthetic-auth-key"}});
        let write = |value: &Value| std::fs::write(directory.path().join("config.json"), value.to_string()).unwrap();
        write(&config);
        let prefs = AgentPrefs::default();
        let auth_key = resolve(&prefs).unwrap();
        assert_eq!(auth_key.provider, "gemini");
        assert_eq!(auth_key.key, "AQ.synthetic-auth-key");
        assert!(auth_key.protocol == Protocol::Gemini);
        assert_eq!(auth_key.recipient.endpoint, "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.8-flash:generateContent");
        assert!(!auth_key.recipient.local);
        assert_eq!(auth_key.model, "gemini-3.8-flash", "fresh Gemini setup uses a model available to new projects");
        assert!(!auth_key.recipient.id.contains(&auth_key.key));

        config["env_vars"]["GEMINI_API_KEY"] = json!("AIzaSyntheticLegacyKey"); write(&config);
        let legacy_key = resolve(&prefs).unwrap();
        assert_eq!(legacy_key.key, "AIzaSyntheticLegacyKey");
        assert_eq!(legacy_key.recipient.endpoint, auth_key.recipient.endpoint);
        assert_ne!(legacy_key.recipient.id, auth_key.recipient.id);
        config["provider"] = json!("google"); write(&config);
        assert_eq!(resolve(&prefs).unwrap().recipient, legacy_key.recipient, "registry aliases resolve to the same Gemini recipient");
        for base in ["https://generativelanguage.googleapis.com/v1beta", "https://generativelanguage.googleapis.com/v1beta/"] {
            config["base_url"] = json!(base); write(&config);
            assert_eq!(resolve(&prefs).unwrap().recipient, legacy_key.recipient, "saved native default supports all input modalities");
        }
        config.as_object_mut().unwrap().remove("base_url");
        config["model"] = json!("gemini-2.5-flash"); write(&config);
        let selected_model = resolve(&prefs).unwrap();
        assert_eq!(selected_model.model, "gemini-2.5-flash", "an explicit legacy model stays selected");
        assert_ne!(selected_model.recipient.id, legacy_key.recipient.id);

        config["base_url"] = json!("https://proxy.example/google/v1/"); write(&config);
        let custom = resolve(&prefs).unwrap();
        assert_eq!(custom.recipient.endpoint, "https://proxy.example/google/v1/chat/completions");
        assert_ne!(custom.recipient.id, selected_model.recipient.id);
        config["api_type"] = json!("anthropic"); write(&config);
        let anthropic = resolve(&prefs).unwrap();
        assert!(anthropic.protocol == Protocol::Anthropic);
        assert_eq!(anthropic.recipient.endpoint, "https://proxy.example/google/v1/messages");
        assert_ne!(anthropic.recipient.id, custom.recipient.id);
        config.as_object_mut().unwrap().remove("base_url"); write(&config);
        assert!(resolve(&prefs).err().unwrap().contains("explicit Anthropic-compatible API base URL"));
        config["api_type"] = json!("native-gemini"); write(&config);
        assert!(resolve(&prefs).err().unwrap().contains("Unsupported model API protocol"));
    }

    #[tokio::test]
    async fn gemini_keys_and_signed_tool_feedback_use_guarded_bearer_transport() {
        for key in ["AQ.synthetic-auth-key", "AIzaSyntheticLegacyKey"] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let allowed = Arc::new(AtomicBool::new(true));
            let mut config = fixture(&format!("http://{address}/v1beta/openai"), Protocol::OpenAi);
            config.provider = "gemini".into(); config.key = key.into();
            let provider = GuardedProvider::new(config, approval(allowed.clone())).unwrap();
            let body = json!({"choices":[{"message":{"content":null,"tool_calls":[{
                "id":"call-1","type":"function","function":{"name":"room_read","arguments":"{\"room\":\"private\"}"},
                "extra_content":{"google":{"thought_signature":"opaque+/signature=","unexpected":"discard"},"other":"discard"}
            }]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":7,"completion_tokens":3}}).to_string();
            let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
            let server = request(listener.try_clone().unwrap(), response, None);
            let result = provider.chat(&[message(MessageRole::User, "approved input")], &[], &ChatConfig::default()).await.unwrap();
            assert_eq!(result.stop_reason, StopReason::ToolUse);
            assert_eq!(result.tool_calls[0].metadata, Some(json!({"thought_signature":"opaque+/signature="})));
            let received = server.join().unwrap();
            assert!(received.starts_with("POST /v1beta/openai/chat/completions HTTP/1.1\r\n"));
            let headers = received.split("\r\n\r\n").next().unwrap();
            let authorization = headers.lines().filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization")).unwrap().1.trim();
            assert_eq!(authorization, format!("Bearer {key}"));
            assert!(!headers.lines().filter_map(|line| line.split_once(':')).any(|(name, _)| name.eq_ignore_ascii_case("x-goog-api-key")));

            let mut assistant = message(MessageRole::Assistant, "");
            assistant.tool_calls = Some(result.tool_calls);
            let mut feedback = message(MessageRole::Tool, "approved private result");
            feedback.tool_call_id = Some("call-1".into());
            let history = [message(MessageRole::User, "approved input"), assistant, feedback];
            let server = request(listener.try_clone().unwrap(), ok_response(), None);
            provider.chat(&history, &[], &ChatConfig::default()).await.unwrap();
            let received = server.join().unwrap();
            let body: Value = serde_json::from_str(received.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(body["messages"][1]["tool_calls"][0]["extra_content"], json!({"google":{"thought_signature":"opaque+/signature="}}));
            assert_eq!(body["messages"][2]["tool_call_id"], "call-1");
            assert_eq!(body["messages"][2]["content"], "approved private result");
            allowed.store(false, Ordering::SeqCst);
            listener.set_nonblocking(true).unwrap();
            let error = provider.chat(&history, &[], &ChatConfig::default()).await.unwrap_err().to_string();
            assert!(error.contains("policy denied"));
            assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        }
    }

    #[test]
    fn gemini_tool_metadata_is_limited_to_string_signatures_and_not_sent_to_other_providers() {
        let response = |signature: Value| json!({"choices":[{"message":{"tool_calls":[{
            "id":"call-1","function":{"name":"room_read","arguments":"{}"},
            "extra_content":{"google":{"thought_signature":signature,"unexpected":"discard"},"other":"discard"}
        }]},"finish_reason":"tool_calls"}],"usage":{}});
        let mut assistant = message(MessageRole::Assistant, "");
        assistant.tool_calls = Some(parse_openai(response(json!("original-signature")), "gemini").unwrap().tool_calls);
        assistant.tool_calls.as_mut().unwrap()[0].metadata.as_mut().unwrap()["unexpected"] = json!("discard");
        let mut config = fixture("https://proxy.example/v1", Protocol::OpenAi);
        config.provider = "gemini".into();
        let body = openai_body(&config, &[assistant.clone()], &[], &ChatConfig::default());
        assert_eq!(body["messages"][0]["tool_calls"][0]["extra_content"], json!({"google":{"thought_signature":"original-signature"}}));
        config.provider = "openai".into();
        let body = openai_body(&config, &[assistant], &[], &ChatConfig::default());
        assert!(body["messages"][0]["tool_calls"][0].get("extra_content").is_none());
        assert!(parse_openai(response(json!("original-signature")), "openai").unwrap().tool_calls[0].metadata.is_none());
        for signature in [Value::Null, json!(""), json!(123), json!({"unexpected":"discard"})] {
            assert!(parse_openai(response(signature), "gemini").unwrap().tool_calls[0].metadata.is_none());
        }
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
    async fn streaming_goes_through_the_same_guarded_request() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let config = fixture(&format!("http://{}/v1", listener.local_addr().unwrap()), Protocol::OpenAi);
        let provider = GuardedProvider::new(config, approval(Arc::new(AtomicBool::new(false)))).unwrap();
        assert!(provider.chat_stream(&[message(MessageRole::User, "private room")], &[], &ChatConfig::default()).await.is_err());
        assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let body = json!({"choices":[{"message":{"content":"Answer","reasoning_content":"Plan first","tool_calls":[{
            "id":"call-1","type":"function","function":{"name":"room_read","arguments":"{}"},
            "extra_content":{"google":{"thought_signature":"signed"}}
        }]},"finish_reason":"tool_calls"}],"usage":{}}).to_string();
        let server = request(listener, format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body), None);
        let mut config = fixture(&format!("http://{address}/v1"), Protocol::OpenAi);
        config.provider = "gemini".into();
        let provider = GuardedProvider::new(config, approval(Arc::new(AtomicBool::new(true)))).unwrap();
        let mut stream = provider.chat_stream(&[message(MessageRole::User, "approved input")], &[], &ChatConfig::default()).await.unwrap();
        let mut events = Vec::new();
        while let Some(event) = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await { events.push(event); }
        assert!(server.join().unwrap().contains("approved input"));
        // The default stream would drop the reasoning and Gemini's signature.
        let position = |found: fn(&octos_llm::StreamEvent) -> bool| events.iter().position(found).unwrap();
        assert!(position(|event| matches!(event, octos_llm::StreamEvent::ReasoningDelta(text) if text == "Plan first"))
            < position(|event| matches!(event, octos_llm::StreamEvent::TextDelta(text) if text == "Answer")));
        assert!(events.iter().any(|event| matches!(event, octos_llm::StreamEvent::ToolCallMetadata { metadata, .. } if metadata["thought_signature"] == "signed")));
    }

    /// Serves one streamed reply, holding after its first event until `release` is set.
    fn stream_server(listener: TcpListener, events: Vec<String>, release: Option<Arc<AtomicBool>>) -> std::thread::JoinHandle<String> {
        stream_server_lingering(listener, events, release, Duration::ZERO)
    }

    /// Like `stream_server`, keeping the connection open for `linger` after the last event.
    fn stream_server_lingering(listener: TcpListener, events: Vec<String>, release: Option<Arc<AtomicBool>>, linger: Duration) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let received = read_request(&mut stream);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nConnection: close\r\n\r\n");
            for (index, event) in events.iter().enumerate() {
                let _ = stream.write_all(format!("data: {event}\n\n").as_bytes());
                let _ = stream.flush();
                if index == 0 && let Some(release) = &release {
                    let deadline = std::time::Instant::now() + Duration::from_secs(5);
                    while !release.load(Ordering::SeqCst) && std::time::Instant::now() < deadline { std::thread::sleep(Duration::from_millis(10)); }
                }
            }
            std::thread::sleep(linger);
            received
        })
    }

    pub(super) type Shown = Arc<std::sync::Mutex<Vec<(bool, String)>>>;

    pub(super) fn previewing(config: Resolved, allowed: Arc<AtomicBool>) -> (Arc<GuardedProvider>, Shown) {
        let shown: Shown = Arc::default();
        let provider = GuardedProvider::new(config, approval(allowed)).unwrap();
        let sink = shown.clone();
        provider.preview.connect(move |is_thought, text| sink.lock().unwrap().push((is_thought, text.into())));
        (Arc::new(provider), shown)
    }

    async fn wait_for(shown: &Shown, count: usize) {
        for _ in 0..500 {
            if shown.lock().unwrap().len() >= count { return; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("previews never arrived: {:?}", shown.lock().unwrap());
    }

    #[tokio::test]
    async fn streamed_reply_is_shown_while_it_arrives_and_returned_whole() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let release = Arc::new(AtomicBool::new(false));
        let events = vec![
            json!({"choices":[{"delta":{"reasoning_content":"Plan "}}]}).to_string(),
            json!({"choices":[{"delta":{"content":"```splash\n"}}]}).to_string(),
            json!({"choices":[{"delta":{"content":"View{}\n```"},"finish_reason":"stop"}]}).to_string(),
            json!({"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":4}}).to_string(),
            "[DONE]".into(),
        ];
        let server = stream_server(listener, events, Some(release.clone()));
        let (provider, shown) = previewing(fixture(&format!("http://{address}/v1"), Protocol::OpenAi), Arc::new(AtomicBool::new(true)));
        let call = tokio::spawn({
            let provider = provider.clone();
            async move { provider.chat(&[message(MessageRole::User, "approved input")], &[], &ChatConfig::default()).await }
        });
        // The first event is on screen while the model is still replying.
        wait_for(&shown, 1).await;
        assert!(!call.is_finished());
        release.store(true, Ordering::SeqCst);
        let response = call.await.unwrap().unwrap();
        assert_eq!(response.content.as_deref(), Some("```splash\nView{}\n```"));
        assert_eq!(response.reasoning_content.as_deref(), Some("Plan "));
        assert_eq!((response.usage.input_tokens, response.usage.output_tokens), (9, 4));
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        assert_eq!(joined(&shown.lock().unwrap()), vec![(true, "Plan ".into()), (false, "```splash\nView{}\n```".into())]);
        let body: Value = serde_json::from_str(server.join().unwrap().split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[tokio::test]
    async fn streamed_tool_calls_keep_their_arguments_and_gemini_signature() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let events = vec![
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"room_read","arguments":"{\"room\""},
                "extra_content":{"google":{"thought_signature":"signed"}}}]}}]}).to_string(),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"private\"}"}}]},"finish_reason":"tool_calls"}]}).to_string(),
            "[DONE]".into(),
        ];
        let server = stream_server(listener, events, None);
        let mut config = fixture(&format!("http://{address}/v1beta/openai"), Protocol::OpenAi);
        config.provider = "gemini".into();
        let (provider, shown) = previewing(config, Arc::new(AtomicBool::new(true)));
        let response = provider.chat(&[message(MessageRole::User, "approved input")], &[], &ChatConfig::default()).await.unwrap();
        server.join().unwrap();
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(response.tool_calls[0].id, "call-1");
        assert_eq!(response.tool_calls[0].arguments, json!({"room":"private"}));
        assert_eq!(response.tool_calls[0].metadata, Some(json!({"thought_signature":"signed"})));
        assert!(shown.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn anthropic_and_gemini_streams_rebuild_their_replies() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let events = vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":7,"output_tokens":1}}}).to_string(),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}).to_string(),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello "}}).to_string(),
            json!({"type":"ping"}).to_string(),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"world"}}).to_string(),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu-1","name":"room_read","input":{}}}).to_string(),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}}).to_string(),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"1}"}}).to_string(),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}).to_string(),
            json!({"type":"message_stop"}).to_string(),
        ];
        let server = stream_server(listener, events, None);
        let (provider, shown) = previewing(fixture(&format!("http://{address}/v1"), Protocol::Anthropic), Arc::new(AtomicBool::new(true)));
        let response = provider.chat(&[message(MessageRole::User, "approved input")], &[], &ChatConfig::default()).await.unwrap();
        assert!(server.join().unwrap().starts_with("POST /v1/messages HTTP/1.1\r\n"));
        assert_eq!(response.content.as_deref(), Some("Hello world"));
        assert_eq!(response.tool_calls[0].arguments, json!({"a":1}));
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!((response.usage.input_tokens, response.usage.output_tokens), (7, 5));
        assert_eq!(joined(&shown.lock().unwrap()), vec![(false, "Hello world".into())]);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let events = vec![
            json!({"candidates":[{"content":{"parts":[{"text":"Thinking it over","thought":true}]}}]}).to_string(),
            json!({"candidates":[{"content":{"parts":[{"text":"Answer"}]}}]}).to_string(),
            json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"room_info","args":{}},"thoughtSignature":"signed"}]},"finishReason":"STOP"}],
                "usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2}}).to_string(),
        ];
        let server = stream_server(listener, events, None);
        let mut config = fixture(&format!("http://{address}/v1beta"), Protocol::Gemini);
        config.recipient.endpoint = format!("http://{address}/v1beta/models/test-model:generateContent");
        let (provider, shown) = previewing(config, Arc::new(AtomicBool::new(true)));
        let response = provider.chat(&[message(MessageRole::User, "approved input")], &[], &ChatConfig::default()).await.unwrap();
        assert!(server.join().unwrap().starts_with("POST /v1beta/models/test-model:streamGenerateContent?alt=sse HTTP/1.1\r\n"));
        assert_eq!(response.reasoning_content.as_deref(), Some("Thinking it over"));
        assert_eq!(response.content.as_deref(), Some("Answer"));
        assert_eq!(response.tool_calls[0].metadata.as_ref().unwrap()["thought_signature"], "signed");
        assert_eq!(joined(&shown.lock().unwrap()), vec![(true, "Thinking it over".into()), (false, "Answer".into())]);
    }

    #[tokio::test]
    async fn revoking_mid_stream_stops_the_reply_and_its_preview() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let release = Arc::new(AtomicBool::new(false));
        let events = vec![
            json!({"choices":[{"delta":{"content":"first"}}]}).to_string(),
            json!({"choices":[{"delta":{"content":"after revocation"},"finish_reason":"stop"}]}).to_string(),
            "[DONE]".into(),
        ];
        let server = stream_server(listener, events, Some(release.clone()));
        let allowed = Arc::new(AtomicBool::new(true));
        let (provider, shown) = previewing(fixture(&format!("http://{address}/v1"), Protocol::OpenAi), allowed.clone());
        let call = tokio::spawn({
            let provider = provider.clone();
            async move { provider.chat(&[message(MessageRole::User, "approved input")], &[], &ChatConfig::default()).await }
        });
        wait_for(&shown, 1).await;
        allowed.store(false, Ordering::SeqCst);
        release.store(true, Ordering::SeqCst);
        assert!(call.await.unwrap().unwrap_err().to_string().contains("policy denied"));
        server.join().unwrap();
        assert_eq!(*shown.lock().unwrap(), vec![(false, "first".into())]);
    }

    #[tokio::test]
    async fn preview_waits_for_influence_and_falls_back_to_one_json_reply() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let events = vec![json!({"choices":[{"delta":{"content":"text"},"finish_reason":"stop"}]}).to_string(), "[DONE]".into()];
        let server = stream_server(listener, events, None);
        let recorded = Arc::new(AtomicBool::new(false));
        let shown: Shown = Arc::default();
        let mut provider = GuardedProvider::new(fixture(&format!("http://{address}/v1"), Protocol::OpenAi), approval(Arc::new(AtomicBool::new(true)))).unwrap();
        provider.on_response = Some({
            let recorded = recorded.clone();
            Arc::new(move || { recorded.store(true, Ordering::SeqCst); Ok(()) })
        });
        // Records whether the influence was already recorded when the text was shown.
        provider.preview.connect({
            let (recorded, shown) = (recorded.clone(), shown.clone());
            move |_, text| shown.lock().unwrap().push((recorded.load(Ordering::SeqCst), text.into()))
        });
        provider.chat(&[message(MessageRole::User, "input")], &[], &ChatConfig::default()).await.unwrap();
        server.join().unwrap();
        assert_eq!(*shown.lock().unwrap(), vec![(true, "text".into())]);

        // A service that ignores the stream request still answers.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = request(listener, ok_response(), None);
        let (provider, shown) = previewing(fixture(&format!("http://{address}/v1"), Protocol::OpenAi), Arc::new(AtomicBool::new(true)));
        let response = provider.chat(&[message(MessageRole::User, "input")], &[], &ChatConfig::default()).await.unwrap();
        server.join().unwrap();
        assert_eq!(response.content.as_deref(), Some("Allowed result"));
        assert!(shown.lock().unwrap().is_empty());

        // The reply is whole at its terminal event, even if the service keeps the connection open.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let events = vec![json!({"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}).to_string(), "[DONE]".into()];
        let server = stream_server_lingering(listener, events, None, Duration::from_secs(3));
        let (provider, _) = previewing(fixture(&format!("http://{address}/v1"), Protocol::OpenAi), Arc::new(AtomicBool::new(true)));
        let started = std::time::Instant::now();
        let response = provider.chat(&[message(MessageRole::User, "input")], &[], &ChatConfig::default()).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(response.content.as_deref(), Some("done"));
        server.join().unwrap();

        // A stream cut off before its end is an error, not a short reply.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = stream_server(listener, vec![json!({"choices":[{"delta":{"content":"half"}}]}).to_string()], None);
        let (provider, _) = previewing(fixture(&format!("http://{address}/v1"), Protocol::OpenAi), Arc::new(AtomicBool::new(true)));
        let error = provider.chat(&[message(MessageRole::User, "input")], &[], &ChatConfig::default()).await.unwrap_err();
        server.join().unwrap();
        assert!(error.to_string().contains("ended early"));
    }

    /// Joins consecutive previews of one kind, as the console shows them.
    pub(super) fn joined(shown: &[(bool, String)]) -> Vec<(bool, String)> {
        let mut joined = Vec::new();
        for (is_thought, text) in shown { push_shown(&mut joined, *is_thought, text); }
        joined
    }

    pub(super) fn sse(events: &[Value]) -> String {
        events.iter().map(|event| format!("data: {event}\n\n")).collect()
    }

    #[test]
    fn floods_of_empty_events_and_fragments_parse_in_linear_time() {
        // Quadratic parsing would hold off revocation checks for minutes here.
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reply = StreamedReply::new(Protocol::OpenAi, "openai");
            let mut flood = b": keep-alive\n".repeat(1 << 14);
            flood.extend(std::iter::repeat_n(b'\n', 1 << 20));
            let calls = vec![json!({"index":0,"function":{"arguments":"x".repeat(100)}}); 50_000];
            let event = json!({"choices":[{"delta":{"tool_calls":calls},"finish_reason":"tool_calls"}]});
            let result = reply.feed(&flood).and_then(|_| reply.feed(format!("data: {event}\n\ndata: [DONE]\n\n").as_bytes()))
                .and_then(|_| reply.finish());
            let _ = done.send(result.map(|value| value["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"].as_str().map(str::len)));
        });
        let arguments = finished.recv_timeout(Duration::from_secs(10)).expect("parsing a flood took too long");
        assert_eq!(arguments.unwrap(), Some(50_000 * 100));
    }

    #[test]
    fn events_survive_any_split_and_crlf_framing() {
        let stream = |newline: &str| [
            ": keep-alive", "",
            &format!("data: {}", json!({"choices":[{"delta":{"reasoning_content":"Plan ✍️ für"}}]})), "",
            "event: message", &format!("data:{}", json!({"choices":[{"delta":{"content":"Hé\nllo 🌍"}}]})), "",
            // One event's data may span several lines.
            "data: {\"choices\":[{\"delta\":", "data: {\"content\":\"!\"},\"finish_reason\":\"stop\"}]}", "",
            "data: [DONE]", "",
        ].map(|line| format!("{line}{newline}")).concat().into_bytes();
        let rebuild = |pieces: Vec<&[u8]>| {
            let mut reply = StreamedReply::new(Protocol::OpenAi, "openai");
            let mut shown = Vec::new();
            for piece in pieces { shown.extend(reply.feed(piece).unwrap()); }
            (joined(&shown), reply.finish().unwrap())
        };
        let lf = stream("\n");
        let expected = rebuild(vec![&lf[..]]);
        assert_eq!(expected.0, vec![(true, "Plan ✍️ für".into()), (false, "Hé\nllo 🌍!".into())]);
        assert_eq!(parse_openai(expected.1.clone(), "openai").unwrap().content.as_deref(), Some("Hé\nllo 🌍!"));
        for at in 0..=lf.len() {
            assert_eq!(rebuild(vec![&lf[..at], &lf[at..]]), expected, "split at {at}");
        }
        let crlf = stream("\r\n");
        assert_eq!(rebuild(crlf.chunks(1).collect()), expected);
    }

    #[test]
    fn anthropic_previews_only_the_text_and_thinking_its_reply_keeps() {
        let rebuild = |events: &[Value]| -> eyre::Result<(Vec<(bool, String)>, Value)> {
            let mut reply = StreamedReply::new(Protocol::Anthropic, "anthropic");
            let shown = reply.feed(sse(events).as_bytes())?;
            Ok((joined(&shown), reply.finish()?))
        };
        let start = |index: u64, block: Value| json!({"type":"content_block_start","index":index,"content_block":block});
        let delta = |index: u64, delta: Value| json!({"type":"content_block_delta","index":index,"delta":delta});
        let text = |text: &str| json!({"type":"text_delta","text":text});
        let thinking = |text: &str| json!({"type":"thinking_delta","thinking":text});
        let stop = json!({"type":"message_stop"});
        // A restarted block, or text on a block that keeps none, would show text the reply drops.
        for events in [
            [start(0, json!({"type":"text","text":""})), delta(0, text("Shown")), start(0, json!({"type":"text","text":""})), stop.clone()],
            [start(0, json!({"type":"tool_use","id":"toolu-1","name":"room_read","input":{}})), delta(0, text("Shown")), stop.clone(), stop.clone()],
            [start(0, json!({"type":"thinking","thinking":""})), delta(0, text("Shown")), stop.clone(), stop.clone()],
            [start(0, json!({"type":"text","text":""})), delta(0, thinking("Shown")), stop.clone(), stop.clone()],
            [start(0, json!({"type":"redacted_thinking","data":"opaque"})), delta(0, thinking("Shown")), stop.clone(), stop.clone()],
        ] {
            assert!(rebuild(&events).is_err(), "{events:?}");
        }
        let (shown, value) = rebuild(&[
            start(0, json!({"type":"thinking","thinking":""})), delta(0, thinking("Plan ")),
            delta(0, json!({"type":"signature_delta","signature":"opaque"})), delta(0, thinking("the layout")),
            start(1, json!({"type":"redacted_thinking","data":"opaque"})),
            start(2, json!({"type":"text","text":"Final "})), delta(2, text("text.")), stop,
        ]).unwrap();
        assert_eq!(shown, vec![(true, "Plan the layout".into()), (false, "Final text.".into())]);
        let response = parse_anthropic(value).unwrap();
        assert_eq!(response.content.as_deref(), Some("Final text."));
        assert_eq!(response.reasoning_content.as_deref(), Some("Plan the layout"));
    }

    #[test]
    fn a_gemini_stream_without_candidates_was_cut_off_unless_blocked() {
        let mut reply = StreamedReply::new(Protocol::Gemini, "gemini");
        reply.feed(sse(&[json!({"usageMetadata":{"promptTokenCount":3}})]).as_bytes()).unwrap();
        assert!(reply.finish().unwrap_err().to_string().contains("ended early"));
        let mut reply = StreamedReply::new(Protocol::Gemini, "gemini");
        reply.feed(sse(&[json!({"promptFeedback":{"blockReason":"SAFETY"}})]).as_bytes()).unwrap();
        let error = super::super::gemini::parse(reply.finish().unwrap()).unwrap_err();
        assert!(error.to_string().contains("safety restrictions"));
    }

    #[tokio::test]
    async fn sse_framing_does_not_count_against_the_reply_size_limit() {
        // About 17 MiB of events that rebuild a 68 KB reply.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let mut events = vec![json!({"id":"x".repeat(1000),"choices":[{"delta":{"content":"abcd"}}]}).to_string(); 17_000];
        events.extend([json!({"choices":[{"delta":{},"finish_reason":"stop"}]}).to_string(), "[DONE]".into()]);
        let server = stream_server(listener, events, None);
        let (provider, _) = previewing(fixture(&format!("http://{address}/v1"), Protocol::OpenAi), Arc::new(AtomicBool::new(true)));
        let response = provider.chat(&[message(MessageRole::User, "input")], &[], &ChatConfig::default()).await.unwrap();
        server.join().unwrap();
        assert_eq!(response.content.map(|content| content.len()), Some(17_000 * 4));

        // The rebuilt reply, and an event still arriving, keep the whole-reply limit.
        let mut reply = StreamedReply::new(Protocol::OpenAi, "openai");
        let event = sse(&[json!({"choices":[{"delta":{"content":"x".repeat(1 << 20)}}]})]);
        assert!((0..17).try_for_each(|_| reply.feed(event.as_bytes()).map(drop)).unwrap_err().to_string().contains("size limit"));
        let mut reply = StreamedReply::new(Protocol::OpenAi, "openai");
        assert!(reply.feed(&vec![b'x'; 17 << 20]).unwrap_err().to_string().contains("size limit"));
    }

    #[tokio::test]
    async fn a_service_that_refuses_to_stream_gets_the_plain_request() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let refusal = json!({"error":{"message":"Your organization must be verified to stream this model.","param":"stream","code":"unsupported_value"}}).to_string();
        let refusal = format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{refusal}", refusal.len());
        let recorded = Arc::new(AtomicBool::new(false));
        let server = std::thread::spawn({
            let (listener, recorded) = (listener.try_clone().unwrap(), recorded.clone());
            move || {
                let streamed = request(listener.try_clone().unwrap(), refusal, None).join().unwrap();
                let (mut stream, _) = listener.accept().unwrap();
                let was_recorded = recorded.load(Ordering::SeqCst);
                let plain = read_request(&mut stream);
                stream.write_all(ok_response().as_bytes()).unwrap();
                (streamed, plain, was_recorded)
            }
        });
        let shown: Shown = Arc::default();
        let mut provider = GuardedProvider::new(fixture(&format!("http://{address}/v1"), Protocol::OpenAi), approval(Arc::new(AtomicBool::new(true)))).unwrap();
        provider.on_response = Some({
            let recorded = recorded.clone();
            Arc::new(move || { recorded.store(true, Ordering::SeqCst); Ok(()) })
        });
        provider.preview.connect({
            let shown = shown.clone();
            move |is_thought, text| shown.lock().unwrap().push((is_thought, text.into()))
        });
        let response = provider.chat(&[message(MessageRole::User, "approved input")], &[], &ChatConfig::default()).await.unwrap();
        assert_eq!(response.content.as_deref(), Some("Allowed result"));
        let (streamed, plain, was_recorded) = server.join().unwrap();
        let body = |received: &str| serde_json::from_str::<Value>(received.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!((body(&streamed)["stream"].clone(), body(&plain)["stream"].clone()), (json!(true), json!(false)));
        assert!(body(&plain).get("stream_options").is_none());
        // The refusal is service output, so its influence is recorded before asking again.
        assert!(was_recorded);

        // Later calls skip the stream the service refused.
        let server = request(listener, ok_response(), None);
        provider.chat(&[message(MessageRole::User, "approved input")], &[], &ChatConfig::default()).await.unwrap();
        assert_eq!(body(&server.join().unwrap())["stream"], false);
        assert!(shown.lock().unwrap().is_empty());
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

#[cfg(test)]
mod multimodal_tests;

#[cfg(test)]
mod streaming_tests;
