//! Discovery of the local Claude Code login, without reading its credentials.
//!
//! The CLI owns subscription authentication. Robrix asks for status metadata
//! and keeps only a private account fingerprint for the model recipient.

use std::{path::{Path, PathBuf}, process::{Command, Stdio}, sync::{Mutex, atomic::{AtomicU64, Ordering}}, time::{Duration, Instant}};
use sha2::{Digest, Sha256};

pub const ID: &str = "claude-code";
pub const LABEL: &str = "Claude Code (your subscription)";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Checking,
    NotInstalled,
    SignedOut,
    Ready,
    Unavailable,
}

#[derive(Clone)]
struct Cache {
    status: Status,
    identity: Option<String>,
    probing: bool,
    generation: u64,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache {
    status: Status::Checking, identity: None, probing: false, generation: 0,
});
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Returns cached status immediately, starting the first probe in a thread.
pub fn status() -> Status {
    if cfg!(any(target_os = "ios", target_os = "android", target_arch = "wasm32")) {
        return Status::Unavailable;
    }
    let mut cache = CACHE.lock().unwrap_or_else(|error| error.into_inner());
    if cache.generation == 0 {
        cache.generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
        cache.probing = true;
        start_probe(cache.generation);
    }
    cache.status
}

/// Rechecks installation and sign-in without hiding a previously ready row.
pub fn refresh() {
    if cfg!(any(target_os = "ios", target_os = "android", target_arch = "wasm32")) { return; }
    let mut cache = CACHE.lock().unwrap_or_else(|error| error.into_inner());
    if cache.probing { return; }
    cache.generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
    cache.probing = true;
    start_probe(cache.generation);
}

fn start_probe(generation: u64) {
    std::thread::spawn(move || {
        let result = probe();
        let mut cache = CACHE.lock().unwrap_or_else(|error| error.into_inner());
        if cache.generation != generation { return; }
        cache.status = result.0;
        cache.identity = result.1;
        cache.probing = false;
        drop(cache);
        makepad_widgets::SignalToUI::set_ui_signal();
    });
}

/// The account binding is private metadata, never an OAuth token or API key.
pub fn account_identity() -> Option<String> {
    if status() != Status::Ready { return None; }
    CACHE.lock().unwrap_or_else(|error| error.into_inner()).identity.clone()
}

/// Verifies the current login before a model request discloses its prompt.
///
/// This performs a bounded CLI probe and belongs on a blocking worker thread.
/// A changed account updates discovery but requires a new recipient approval.
pub fn verify_account(expected: &str) -> Result<(), String> {
    let result = probe();
    let verified = result.0 == Status::Ready && result.1.as_deref() == Some(expected);
    let mut cache = CACHE.lock().unwrap_or_else(|error| error.into_inner());
    cache.generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
    cache.status = result.0;
    cache.identity = result.1;
    cache.probing = false;
    drop(cache);
    makepad_widgets::SignalToUI::set_ui_signal();
    if verified { Ok(()) }
    else { Err("Claude Code sign-in changed or is unavailable. Check sign-in, restart the agent, and approve the current Claude Code account.".into()) }
}

/// Finds the installed CLI even when a GUI launch inherits a short PATH.
pub fn executable() -> Option<PathBuf> {
    if cfg!(any(target_os = "ios", target_os = "android", target_arch = "wasm32")) { return None; }
    let mut candidates = Vec::new();
    if let Some(explicit) = std::env::var_os("CLAUDE_CODE_EXECUTABLE").filter(|value| !value.is_empty()) {
        return executable_path(PathBuf::from(explicit));
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|directory| directory.join("claude")));
    }
    if let Some(home) = home_directory() {
        candidates.extend(home_executable_candidates(&home, cfg!(target_os = "windows")));
    }
    candidates.extend([PathBuf::from("/opt/homebrew/bin/claude"), PathBuf::from("/usr/local/bin/claude")]);
    #[cfg(target_os = "windows")]
    {
        if let Some(path) = std::env::var_os("PATH") {
            candidates.extend(std::env::split_paths(&path).flat_map(|directory| [directory.join("claude.exe"), directory.join("claude.cmd")]));
        }
        if let Some(appdata) = std::env::var_os("APPDATA") {
            candidates.push(PathBuf::from(appdata).join("npm/claude.cmd"));
        }
    }
    candidates.into_iter().find_map(executable_path)
}

fn home_directory() -> Option<PathBuf> {
    resolve_home_directory(std::env::var_os("HOME"), std::env::var_os("USERPROFILE"))
}

fn resolve_home_directory(home: Option<std::ffi::OsString>, user_profile: Option<std::ffi::OsString>) -> Option<PathBuf> {
    home.or(user_profile).map(PathBuf::from)
}

fn home_executable_candidates(home: &Path, windows: bool) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if windows { candidates.push(home.join(".local/bin/claude.exe")); }
    candidates.extend([
        home.join(".local/bin/claude"), home.join(".claude/local/claude"),
        home.join(".npm-global/bin/claude"),
    ]);
    candidates
}

fn executable_path(path: PathBuf) -> Option<PathBuf> {
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() { return None; }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 { return None; }
    }
    path.canonicalize().ok()
}

/// Builds a subscription-only child while preserving the user's normal login.
pub fn command() -> Result<Command, String> {
    check_managed_policy()?;
    let path = executable().ok_or("Install Claude Code before using your subscription.")?;
    Ok(subscription_command(&path))
}

fn subscription_command(path: &Path) -> Command {
    let mut command = Command::new(path);
    command.env_clear();
    for name in [
        "HOME", "PATH", "TMPDIR", "TMP", "TEMP", "USER", "LOGNAME", "LANG", "LC_ALL", "LC_CTYPE",
        "SystemRoot", "WINDIR", "APPDATA", "LOCALAPPDATA", "USERPROFILE", "CLAUDE_CONFIG_DIR",
        "XDG_CONFIG_HOME", "XDG_CACHE_HOME",
    ] {
        if let Some(value) = std::env::var_os(name) { command.env(name, value); }
    }
    command.env("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1")
        .env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1")
        .env("CLAUDE_CODE_AUTO_CONNECT_IDE", "false")
        .env("CLAUDE_CODE_DISABLE_ATTACHMENTS", "1")
        .env("DISABLE_AUTOUPDATER", "1");
    command
}

fn probe() -> (Status, Option<String>) {
    if check_managed_policy().is_err() { return (Status::Unavailable, None); }
    let Some(path) = executable() else {
        return (if cfg!(any(target_os = "ios", target_os = "android", target_arch = "wasm32")) { Status::Unavailable } else { Status::NotInstalled }, None);
    };
    probe_executable(&path, Duration::from_secs(4))
}

/// Managed hooks and routing overrides cannot be disabled by CLI flags.
pub fn check_managed_policy() -> Result<(), String> {
    let directory = if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/ClaudeCode")
    } else if cfg!(target_os = "windows") {
        std::env::var_os("ProgramFiles").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:/Program Files")).join("ClaudeCode")
    } else {
        PathBuf::from("/etc/claude-code")
    };
    let mut paths = vec![directory.join("managed-settings.json"), directory.join("managed-mcp.json")];
    let config_directory = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from)
        .or_else(|| home_directory().map(|home| home.join(".claude")));
    if let Some(directory) = config_directory { paths.push(directory.join("remote-settings.json")); }
    for path in paths {
        let bytes = match std::fs::read(&path) {
            Ok(bytes) if bytes.len() <= 65536 => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            _ => return Err("Claude Code managed policy could not be checked safely.".into()),
        };
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| "Claude Code managed policy could not be checked safely.")?;
        validate_managed_policy(&value)?;
    }
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("/usr/bin/id");
        command.arg("-un");
        let (_, bytes) = bounded_output(command, Duration::from_millis(250))
            .filter(|(success, _)| *success).ok_or("Claude Code managed policy could not be checked safely.")?;
        let username = std::str::from_utf8(&bytes).map_err(|_| "Claude Code managed policy could not be checked safely.")?.trim();
        if username.is_empty() || username.contains(['/', '\\']) || username.chars().any(|character| character.is_control()) {
            return Err("Claude Code managed policy could not be checked safely.".into());
        }
        let base = Path::new("/Library/Managed Preferences");
        for path in [base.join(username).join("com.anthropic.claudecode.plist"), base.join("com.anthropic.claudecode.plist")] {
            if !path.exists() { continue; }
            let mut command = Command::new("/usr/bin/plutil");
            command.args(["-convert", "json", "-o", "-"]).arg(&path);
            let (_, bytes) = bounded_output(command, Duration::from_millis(250))
                .filter(|(success, _)| *success).ok_or("Claude Code managed policy could not be checked safely.")?;
            let value: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|_| "Claude Code managed policy could not be checked safely.")?;
            validate_managed_policy(&value)?;
        }
    }
    Ok(())
}

fn validate_managed_policy(value: &serde_json::Value) -> Result<(), String> {
    let object = value.as_object().ok_or("Claude Code managed policy could not be checked safely.")?;
    for name in ["hooks", "mcpServers", "apiKeyHelper", "awsAuthRefresh", "awsCredentialExport"] {
        if let Some(value) = object.get(name) {
            let empty = value.is_null() || value.as_object().is_some_and(|value| value.is_empty())
                || value.as_array().is_some_and(|value| value.is_empty()) || value.as_str() == Some("");
            if !empty { return Err("Claude Code managed hooks, MCP servers, or authentication helpers cannot run in a protected Robrix session.".into()); }
        }
    }
    if let Some(env) = object.get("env") {
        let env = env.as_object().ok_or("Claude Code managed environment could not be checked safely.")?;
        if env.keys().any(|name| {
            let name = name.to_ascii_uppercase();
            name.starts_with("ANTHROPIC_") || name.starts_with("AWS_") || name.starts_with("AZURE_")
                || name.starts_with("GOOGLE_") || name.starts_with("CLAUDE_CODE_USE_")
                || matches!(name.as_str(), "CLAUDE_CODE_OAUTH_TOKEN" | "CLAUDE_CODE_SIMPLE" | "HTTP_PROXY" | "HTTPS_PROXY" | "ALL_PROXY")
        }) { return Err("Claude Code managed routing overrides cannot run in a protected Robrix session.".into()); }
    }
    Ok(())
}

fn probe_executable(path: &Path, timeout: Duration) -> (Status, Option<String>) {
    let mut command = subscription_command(path);
    command.args(["--setting-sources", "", "--settings", "{\"disableAllHooks\":true,\"autoMemoryEnabled\":false}", "auth", "status", "--json"]);
    let Some((success, bytes)) = bounded_output(command, timeout) else { return (Status::Unavailable, None); };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return (Status::Unavailable, None); };
    status_from_json(success, &value)
}

/// Identifies an installed build, so a changed binary is never taken for an approved one.
pub(crate) fn executable_stamp(path: &Path) -> Result<String, String> {
    let metadata = std::fs::metadata(path).map_err(|_| "Claude Code installation changed. Restart the agent.")?;
    if !metadata.is_file() { return Err("Claude Code executable is unavailable.".into()); }
    let modified = metadata.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map(|time| time.as_nanos()).unwrap_or_default();
    #[cfg(unix)]
    let stamp = { use std::os::unix::fs::MetadataExt; format!("{}:{}:{}:{}:{}", metadata.dev(), metadata.ino(), metadata.len(), modified, metadata.mode()) };
    #[cfg(not(unix))]
    let stamp = format!("{}:{}", metadata.len(), modified);
    Ok(stamp)
}

/// What a CLI build offers a live preview.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PreviewSupport {
    /// Its help lists partial messages. Without them a call keeps the single JSON reply.
    pub are_partial_messages: bool,
    /// It takes the SDK's `--thinking-display`, which its help hides.
    pub is_thinking_display: bool,
    /// What updating it would add, when its help or version shows it is too old.
    pub update: Option<Update>,
}

/// What a newer CLI would show live, with the installed version.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Update {
    /// It can't stream the model's output.
    Streaming(Option<(u32, u32, u32)>),
    /// It streams, but not newer models' thinking.
    Thinking((u32, u32, u32)),
}

/// The oldest build known to take `--thinking-display`.
const THINKING_DISPLAY_VERSION: (u32, u32, u32) = (2, 1, 246);

/// What recent builds offer, by path and stamp.
static CHECKED: Mutex<Vec<(PathBuf, String, PreviewSupport)>> = Mutex::new(Vec::new());

fn checked(path: &Path, stamp: &str) -> Option<PreviewSupport> {
    CHECKED.lock().unwrap_or_else(|error| error.into_inner()).iter()
        .find(|(checked, checked_stamp, _)| checked == path && checked_stamp == stamp).map(|(.., support)| *support)
}

fn remember(path: &Path, stamp: &str, support: PreviewSupport) {
    let mut checked = CHECKED.lock().unwrap_or_else(|error| error.into_inner());
    checked.retain(|(checked, checked_stamp, _)| checked != path || checked_stamp != stamp);
    if checked.len() >= 8 { checked.remove(0); }
    checked.push((path.to_path_buf(), stamp.to_string(), support));
}

/// What this CLI build offers a live preview, read once per installed binary.
/// No clear answer from its help keeps the single JSON reply.
pub fn preview_support(path: &Path, stamp: &str) -> PreviewSupport {
    // One probe at a time, so a generation's notice and its first call share it.
    static PROBING: Mutex<()> = Mutex::new(());
    if let Some(support) = checked(path, stamp) { return support; }
    let _probing = PROBING.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(support) = checked(path, stamp) { return support; }
    let Some(are_partial_messages) = are_partial_messages_listed(path, Duration::from_secs(4)) else { return PreviewSupport::default(); };
    let version = version(path, Duration::from_secs(4));
    let is_thinking_display = are_partial_messages && version.is_some_and(|version| version >= THINKING_DISPLAY_VERSION);
    let update = if !are_partial_messages { Some(Update::Streaming(version)) }
        else { version.filter(|_| !is_thinking_display).map(Update::Thinking) };
    let support = PreviewSupport { are_partial_messages, is_thinking_display, update };
    remember(path, stamp, support);
    support
}

/// Keeps what is left after this build refused part of a live preview, so later calls skip it.
pub(crate) fn preview_refused(path: &Path, stamp: &str, support: PreviewSupport) {
    remember(path, stamp, support);
}

/// What updating the installed CLI would show live, read off the UI thread.
pub(crate) fn preview_update() -> Option<Update> {
    check_managed_policy().ok()?;
    let path = executable()?;
    preview_support(&path, &executable_stamp(&path).ok()?).update
}

/// The build's version, from `--version` output like "2.1.63 (Claude Code)".
fn version(path: &Path, timeout: Duration) -> Option<(u32, u32, u32)> {
    let mut command = subscription_command(path);
    command.arg("--version");
    let (_, output) = bounded_output(command, timeout).filter(|(success, _)| *success)?;
    let mut numbers = std::str::from_utf8(&output).ok()?.split_whitespace().next()?.split(['.', '-']).map(|number| number.parse().ok());
    Some((numbers.next()??, numbers.next()??, numbers.next()??))
}

fn are_partial_messages_listed(path: &Path, timeout: Duration) -> Option<bool> {
    const OPTION: &[u8] = b"--include-partial-messages";
    let mut command = subscription_command(path);
    command.arg("--help");
    let (success, help) = bounded_output(command, timeout)?;
    Some(success && help.windows(OPTION.len()).any(|window| window == OPTION))
}

fn bounded_output(mut command: Command, timeout: Duration) -> Option<(bool, Vec<u8>)> {
    use std::io::{Read, Seek, SeekFrom};
    // Anonymous output avoids pipe deadlocks and has a strict size limit.
    let mut output = tempfile::tempfile().ok()?;
    let child_output = output.try_clone().ok()?;
    command.stdin(Stdio::null()).stdout(Stdio::from(child_output)).stderr(Stdio::null());
    let mut child = command.spawn().ok()?;
    let started = Instant::now();
    let exit = loop {
        match child.try_wait() {
            Ok(Some(exit)) => break exit,
            Ok(None) if started.elapsed() < timeout && output.metadata().is_ok_and(|metadata| metadata.len() <= 65536) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    output.seek(SeekFrom::Start(0)).ok()?;
    let mut bytes = Vec::new();
    if output.take(65537).read_to_end(&mut bytes).is_err() || bytes.len() > 65536 {
        return None;
    }
    Some((exit.success(), bytes))
}

fn status_from_json(success: bool, value: &serde_json::Value) -> (Status, Option<String>) {
    if value.get("loggedIn").and_then(|value| value.as_bool()) == Some(false) {
        return (Status::SignedOut, None);
    }
    let method = value.get("authMethod").and_then(|value| value.as_str());
    if !success || value.get("loggedIn").and_then(|value| value.as_bool()) != Some(true)
        || !matches!(method, Some("claude.ai" | "oauth_token"))
        || value.get("apiProvider").and_then(|value| value.as_str()).is_some_and(|provider| provider != "firstParty")
        || value.get("subscriptionType").and_then(|value| value.as_str()).is_some_and(|plan| matches!(plan, "team" | "enterprise"))
    {
        return (Status::Unavailable, None);
    }
    let mut hash = Sha256::new();
    let mut has_account = false;
    for name in ["authMethod", "subscriptionType", "email", "accountId", "organizationId", "orgId"] {
        if let Some(value) = value.get(name).and_then(|value| value.as_str()).filter(|value| !value.is_empty()) {
            if !matches!(name, "authMethod" | "subscriptionType") { has_account = true; }
            hash.update((name.len() as u64).to_le_bytes());
            hash.update(name.as_bytes());
            hash.update((value.len() as u64).to_le_bytes());
            hash.update(value.as_bytes());
        }
    }
    if !has_account { return (Status::Unavailable, None); }
    (Status::Ready, Some(format!("{:x}", hash.finalize())))
}

#[cfg(test)]
pub(crate) struct TestStatusGuard(Cache);

#[cfg(test)]
impl Drop for TestStatusGuard {
    fn drop(&mut self) {
        let mut cache = CACHE.lock().unwrap_or_else(|error| error.into_inner());
        *cache = self.0.clone();
        cache.generation = if cache.generation == 0 || cache.probing { 0 }
            else { NEXT_GENERATION.fetch_add(1, Ordering::Relaxed) };
        cache.probing = false;
    }
}

/// Tests must hold CONFIG_ENV_LOCK for the guard's lifetime.
#[cfg(test)]
pub(crate) fn test_status(status: Status) -> TestStatusGuard {
    let mut cache = CACHE.lock().unwrap_or_else(|error| error.into_inner());
    let previous = cache.clone();
    cache.generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
    cache.status = status;
    cache.identity = (status == Status::Ready).then(|| "fixture-account-identity".into());
    cache.probing = false;
    TestStatusGuard(previous)
}

/// A fixture `claude` in `directory` that runs the shell `body`, kept beside it in `fixture.sh`.
/// All fixtures link to one launcher: a malware scanner can hold up each new executable's first run for seconds.
#[cfg(all(test, unix))]
pub(crate) fn fixture_cli(directory: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    static LAUNCHER: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    unsafe extern "C" { fn atexit(callback: extern "C" fn()) -> i32; }
    extern "C" fn remove_launcher() {
        if let Some(directory) = LAUNCHER.get().and_then(|launcher| launcher.parent()) { let _ = std::fs::remove_dir_all(directory); }
    }
    let launcher = LAUNCHER.get_or_init(|| {
        let launcher = tempfile::Builder::new().prefix("robrix-fixture-cli-").tempdir().unwrap().keep().join("claude");
        std::fs::write(&launcher, "#!/bin/sh\n. \"${0%/*}/fixture.sh\"\n").unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Its first run happens here, outside any test's timing.
        std::fs::write(launcher.with_file_name("fixture.sh"), "exit 0\n").unwrap();
        assert!(Command::new(&launcher).status().unwrap().success());
        unsafe { atexit(remove_launcher); }
        launcher
    });
    std::fs::write(directory.join("fixture.sh"), body).unwrap();
    let path = directory.join("claude");
    std::fs::hard_link(launcher, &path).unwrap();
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_windows_install_is_found_without_home_or_an_updated_path() {
        let directory = tempfile::tempdir().unwrap();
        let home = resolve_home_directory(None, Some(directory.path().as_os_str().into())).unwrap();
        let native_cli = directory.path().join(".local/bin/claude.exe");
        std::fs::create_dir_all(native_cli.parent().unwrap()).unwrap();
        std::fs::write(&native_cli, "fixture executable").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&native_cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let found = home_executable_candidates(&home, true).into_iter().find_map(executable_path);
        assert_eq!(found, Some(native_cli.canonicalize().unwrap()));
        assert_eq!(resolve_home_directory(Some("preferred-home".into()), Some("fallback-profile".into())), Some(PathBuf::from("preferred-home")));
        assert_eq!(resolve_home_directory(None, None), None);
    }

    #[test]
    fn status_requires_a_subscription_and_private_account_binding() {
        for value in [json!({"loggedIn":true,"authMethod":"api_key","email":"fixture@example.org"}),
            json!({"loggedIn":true,"authMethod":"claude.ai","apiProvider":"bedrock","email":"fixture@example.org"}),
            json!({"loggedIn":true,"authMethod":"claude.ai","subscriptionType":"team","email":"fixture@example.org"}),
            json!({"loggedIn":true,"authMethod":"claude.ai","subscriptionType":"enterprise","email":"fixture@example.org"}),
            json!({"loggedIn":true,"authMethod":"claude.ai"}), json!({})]
        {
            assert_eq!(status_from_json(true, &value), (Status::Unavailable, None));
        }
        assert_eq!(status_from_json(false, &json!({"loggedIn":false})), (Status::SignedOut, None));
        let first = status_from_json(true, &json!({"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"first@example.org"}));
        let second = status_from_json(true, &json!({"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"second@example.org"}));
        assert_eq!(first.0, Status::Ready);
        assert_ne!(first.1, second.1);
        assert!(!first.1.unwrap().contains("first@example.org"));
    }

    #[test]
    fn managed_hooks_and_routing_cannot_override_isolation() {
        assert!(validate_managed_policy(&json!({"hooks":{},"mcpServers":{}})).is_ok());
        for policy in [json!({"hooks":{"SessionStart":[{"command":"fixture"}]}}),
            json!({"mcpServers":{"fixture":{"command":"fixture"}}}),
            json!({"apiKeyHelper":"fixture"}), json!({"env":{"ANTHROPIC_BASE_URL":"https://fixture.invalid"}})]
        {
            assert!(validate_managed_policy(&policy).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn fake_cli_probe_is_bounded_and_parses_auth_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let executable = fixture_cli(directory.path(), "printf '%s' '{\"loggedIn\":true,\"authMethod\":\"claude.ai\",\"email\":\"fixture@example.org\"}'\n");
        let rewrite = |body: &str| std::fs::write(directory.path().join("fixture.sh"), body).unwrap();
        assert_eq!(probe_executable(&executable, Duration::from_secs(1)).0, Status::Ready);
        rewrite("exec /bin/sleep 10\n");
        let start = Instant::now();
        assert_eq!(probe_executable(&executable, Duration::from_millis(100)), (Status::Unavailable, None));
        assert!(start.elapsed() < Duration::from_secs(2));
        rewrite("printf '%s' 'invalid json'\n");
        assert_eq!(probe_executable(&executable, Duration::from_secs(1)), (Status::Unavailable, None));
    }

    #[cfg(unix)]
    #[test]
    fn preview_support_is_read_from_help_and_version_once_per_binary() {
        let directory = tempfile::tempdir().unwrap();
        let asked = directory.path().join("asked");
        let cli_answering = |help: &str, version: &str| format!("printf '%s ' \"$1\" >> '{}'\ncase \"$1\" in --version) printf '%s\\n' '{version}' ;; *) printf '%s\\n' '{help}' ;; esac\n", asked.display());
        let partial = "  --include-partial-messages  Include partial message chunks as they arrive";
        let cli = fixture_cli(directory.path(), &cli_answering(partial, "2.1.301 (Claude Code)"));
        let rewrite = |body: &str| std::fs::write(directory.path().join("fixture.sh"), body).unwrap();
        let both = PreviewSupport { are_partial_messages: true, is_thinking_display: true, update: None };
        assert_eq!(preview_support(&cli, "first-build"), both);
        assert_eq!(preview_support(&cli, "first-build"), both);
        assert_eq!(std::fs::read_to_string(&asked).unwrap(), "--help --version ");
        // A changed build is asked again. The hidden display option came after partial messages,
        // so an older build is worth updating, but one whose version is unclear is not.
        let only_partial = |update| PreviewSupport { are_partial_messages: true, is_thinking_display: false, update };
        for (stamp, version, expected) in [("second-build", "2.1.63 (Claude Code)", only_partial(Some(Update::Thinking((2, 1, 63))))),
            ("third-build", "2.1.246 (Claude Code)", both), ("fourth-build", "3.0.0-beta.1 (Claude Code)", both),
            ("fifth-build", "unknown", only_partial(None))] {
            rewrite(&cli_answering(partial, version));
            assert_eq!(preview_support(&cli, stamp), expected, "{version}");
        }
        // One without partial messages keeps the single reply, and its version names what to update.
        let older = |update| PreviewSupport { update: Some(Update::Streaming(update)), ..Default::default() };
        for (stamp, version, expected) in [("sixth-build", "1.0.30 (Claude Code)", older(Some((1, 0, 30)))), ("seventh-build", "unknown", older(None))] {
            std::fs::write(&asked, "").unwrap();
            rewrite(&cli_answering("  --output-format <format>  Output format", version));
            assert_eq!(preview_support(&cli, stamp), expected, "{version}");
            assert_eq!(std::fs::read_to_string(&asked).unwrap(), "--help --version ");
        }
        // What a build refused stays refused, without asking it again.
        preview_refused(&cli, "first-build", PreviewSupport { is_thinking_display: false, ..both });
        assert_eq!(preview_support(&cli, "first-build"), only_partial(None));
        assert_eq!(std::fs::read_to_string(&asked).unwrap(), "--help --version ");
        // A help that never finishes is no answer.
        rewrite("exec /bin/sleep 10\n");
        let started = Instant::now();
        assert_eq!(are_partial_messages_listed(&cli, Duration::from_millis(100)), None);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn discovery_refresh_is_nonblocking_and_account_changes_require_new_approval() {
        let _guard = crate::CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let _status = test_status(Status::Checking);
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe { match &self.0 {
                    Some(value) => std::env::set_var("CLAUDE_CODE_EXECUTABLE", value),
                    None => std::env::remove_var("CLAUDE_CODE_EXECUTABLE"),
                } }
            }
        }
        let _restore = Restore(std::env::var_os("CLAUDE_CODE_EXECUTABLE"));
        let directory = tempfile::tempdir().unwrap();
        let status_body = |value: serde_json::Value| format!("printf '%s' '{value}'\n");
        let cli = fixture_cli(directory.path(), &status_body(json!({"loggedIn":true,"authMethod":"claude.ai","email":"first@example.org"})));
        let write_status = |value: serde_json::Value| std::fs::write(directory.path().join("fixture.sh"), status_body(value)).unwrap();
        unsafe { std::env::set_var("CLAUDE_CODE_EXECUTABLE", &cli); }
        let wait_for = |expected| {
            let started = Instant::now();
            while status() != expected && started.elapsed() < Duration::from_secs(2) {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(status(), expected);
        };
        let started = Instant::now();
        refresh();
        assert!(started.elapsed() < Duration::from_millis(100));
        wait_for(Status::Ready);
        let first = account_identity().unwrap();
        assert!(verify_account(&first).is_ok());
        assert_eq!(command().unwrap().get_program(), cli.canonicalize().unwrap());
        write_status(json!({"loggedIn":true,"authMethod":"claude.ai","email":"second@example.org"}));
        assert!(verify_account(&first).is_err());
        assert_eq!(status(), Status::Ready);
        assert_ne!(account_identity().unwrap(), first);
        write_status(json!({"loggedIn":false}));
        refresh();
        wait_for(Status::SignedOut);
        assert!(account_identity().is_none());
        std::fs::remove_file(&cli).unwrap();
        assert!(executable().is_none(), "an explicit missing CLI cannot silently choose another installation");
        refresh();
        wait_for(Status::NotInstalled);
    }

    #[cfg(unix)]
    #[test]
    fn subscription_child_removes_billing_routing_and_preserves_login_directory() {
        let _guard = crate::CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let names = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL", "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_SIMPLE", "HTTPS_PROXY", "CLAUDE_CONFIG_DIR", "OPENAI_API_KEY", "KIMI_CODING_API_KEY", "ROBRIX_CUSTOM_CREDENTIAL", "MAX_THINKING_TOKENS", "CLAUDE_CODE_EFFORT_LEVEL"];
        struct Restore(Vec<(&'static str, Option<std::ffi::OsString>)>);
        impl Drop for Restore {
            fn drop(&mut self) {
                for (name, value) in &self.0 {
                    unsafe { match value { Some(value) => std::env::set_var(name, value), None => std::env::remove_var(name) } }
                }
            }
        }
        let _restore = Restore(names.iter().map(|name| (*name, std::env::var_os(name))).collect());
        unsafe { for name in names { std::env::set_var(name, "fixture-override"); } }
        let mut command = subscription_command(Path::new("/bin/sh"));
        let output = command.args(["-c", "test -z \"$ANTHROPIC_API_KEY$ANTHROPIC_AUTH_TOKEN$ANTHROPIC_BASE_URL$CLAUDE_CODE_OAUTH_TOKEN$CLAUDE_CODE_USE_BEDROCK$CLAUDE_CODE_SIMPLE$HTTPS_PROXY$OPENAI_API_KEY$KIMI_CODING_API_KEY$ROBRIX_CUSTOM_CREDENTIAL$MAX_THINKING_TOKENS$CLAUDE_CODE_EFFORT_LEVEL\" && test \"$CLAUDE_CONFIG_DIR\" = fixture-override && test \"$CLAUDE_CODE_DISABLE_CLAUDE_MDS\" = 1 && test \"$CLAUDE_CODE_DISABLE_AUTO_MEMORY\" = 1 && test \"$DISABLE_AUTOUPDATER\" = 1"]).status().unwrap();
        assert!(output.success());
    }
}
