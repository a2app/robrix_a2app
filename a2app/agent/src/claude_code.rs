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
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("claude");
        std::fs::write(&executable, "#!/bin/sh\nprintf '%s' '{\"loggedIn\":true,\"authMethod\":\"claude.ai\",\"email\":\"fixture@example.org\"}'\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(probe_executable(&executable, Duration::from_secs(1)).0, Status::Ready);
        std::fs::write(&executable, "#!/bin/sh\nexec /bin/sleep 10\n").unwrap();
        let start = Instant::now();
        assert_eq!(probe_executable(&executable, Duration::from_millis(100)), (Status::Unavailable, None));
        assert!(start.elapsed() < Duration::from_secs(2));
        std::fs::write(&executable, "#!/bin/sh\nprintf '%s' 'invalid json'\n").unwrap();
        assert_eq!(probe_executable(&executable, Duration::from_secs(1)), (Status::Unavailable, None));
    }

    #[cfg(unix)]
    #[test]
    fn discovery_refresh_is_nonblocking_and_account_changes_require_new_approval() {
        use std::os::unix::fs::PermissionsExt;
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
        let cli = directory.path().join("claude");
        let write_status = |value: serde_json::Value| {
            std::fs::write(&cli, format!("#!/bin/sh\nprintf '%s' '{}'\n", value)).unwrap();
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        };
        write_status(json!({"loggedIn":true,"authMethod":"claude.ai","email":"first@example.org"}));
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
