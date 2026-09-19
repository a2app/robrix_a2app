//! Model recipients are bound to the configured endpoint, model, and credential.
//!
//! Protected agents use a single host-owned HTTP transport. It checks current
//! source policies for every request, including tool feedback and compaction;
//! neither provider fallback nor redirects may silently change the recipient.

use crate::prefs::AgentPrefs;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRecipient {
    pub id: String,
    pub label: String,
    pub endpoint: String,
    /// Descriptive only: a loopback service can itself forward data elsewhere.
    pub local: bool,
}

pub fn current_recipient(prefs: &AgentPrefs) -> Result<ModelRecipient, String> {
    guarded::resolve(prefs).map(|config| config.recipient)
}

/// Canonical Octos auth-store path shared by setup discovery and the transport.
pub(crate) fn auth_store_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME").filter(|value| !value.is_empty()).map(std::path::PathBuf::from);
    let directory = if let Some(directory) = std::env::var_os("OCTOS_CONFIG_DIR").filter(|value| !value.is_empty()) {
        let directory = std::path::PathBuf::from(directory);
        if directory == std::path::Path::new("~") { home? }
        else if let Ok(tail) = directory.strip_prefix("~/") { home?.join(tail) }
        else { directory }
    } else {
        #[cfg(unix)]
        {
            let xdg = std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from).filter(|path| path.is_absolute());
            xdg.or_else(|| home.map(|home| home.join(".config")))?.join("octos")
        }
        #[cfg(not(unix))]
        {
            std::path::PathBuf::from(std::env::var_os("APPDATA").filter(|value| !value.is_empty())?).join("octos")
        }
    };
    Some(directory.join("auth.json"))
}

#[derive(serde::Deserialize)]
struct AuthData { credentials: std::collections::BTreeMap<String, AuthCredential> }

#[derive(serde::Deserialize)]
struct AuthCredential {
    access_token: String,
    #[serde(default)]
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn auth_credentials() -> Option<std::collections::BTreeMap<String, AuthCredential>> {
    let path = auth_store_path()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(&path) {
            if metadata.permissions().mode() & 0o077 != 0 {
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
    let mut data: AuthData = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let now = chrono::Utc::now();
    data.credentials.retain(|_, credential| !credential.access_token.is_empty()
        && credential.expires_at.is_none_or(|expiry| expiry >= now));
    Some(data.credentials)
}

pub(crate) fn auth_store_provider() -> Option<String> {
    let credentials = auth_credentials()?;
    if credentials.contains_key("anthropic") { Some("anthropic".into()) }
    else { credentials.into_keys().next() }
}

fn stored_api_key(provider: &str) -> Option<String> {
    Some(auth_credentials()?.remove(provider)?.access_token)
}

mod guarded;
pub(crate) use guarded::provider;

#[cfg(test)]
mod tests {
    use super::*;

    struct Restore(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl Restore {
        fn capture(names: &[&'static str]) -> Self {
            Self(names.iter().map(|name| (*name, std::env::var_os(name))).collect())
        }
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            for (name, value) in &self.0 {
                unsafe { match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                } }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn auth_store_path_uses_one_explicit_or_canonical_directory() {
        let _guard = crate::CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let _restore = Restore::capture(&["OCTOS_CONFIG_DIR", "XDG_CONFIG_HOME"]);
        let directory = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", directory.path());
            std::env::set_var("OCTOS_CONFIG_DIR", "");
        }
        assert_eq!(auth_store_path(), Some(directory.path().join("octos/auth.json")));
        unsafe { std::env::set_var("OCTOS_CONFIG_DIR", directory.path().join("explicit")); }
        assert_eq!(auth_store_path(), Some(directory.path().join("explicit/auth.json")));
        assert_eq!(crate::provider_from_auth_store(), None, "an absent explicit file cannot fall back to another store");
        if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()).map(std::path::PathBuf::from) {
            unsafe { std::env::set_var("OCTOS_CONFIG_DIR", "~/fixture-auth-path"); }
            assert_eq!(auth_store_path(), Some(home.join("fixture-auth-path/auth.json")));
            unsafe { std::env::set_var("OCTOS_CONFIG_DIR", "~"); }
            assert_eq!(auth_store_path(), Some(home.join("auth.json")));
            unsafe {
                std::env::remove_var("OCTOS_CONFIG_DIR");
                std::env::set_var("XDG_CONFIG_HOME", "relative-is-not-an-xdg-root");
            }
            assert_eq!(auth_store_path(), Some(home.join(".config/octos/auth.json")));
        }
    }

    #[test]
    fn discovery_and_transport_reject_the_same_empty_or_expired_credentials() {
        let _guard = crate::CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let _restore = Restore::capture(&["OCTOS_CONFIG_DIR"]);
        let directory = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("OCTOS_CONFIG_DIR", directory.path()); }
        let write = |credentials: serde_json::Value| std::fs::write(
            directory.path().join("auth.json"), serde_json::json!({"credentials": credentials}).to_string(),
        ).unwrap();
        write(serde_json::json!({
            "anthropic": {"access_token":"expired", "expires_at":"2000-01-01T00:00:00Z"},
            "groq": {"access_token":""},
            "openai": {"access_token":"current", "expires_at":"2999-01-01T00:00:00Z"},
        }));
        assert_eq!(crate::provider_from_auth_store().as_deref(), Some("openai"));
        assert_eq!(stored_api_key("openai").as_deref(), Some("current"));
        assert_eq!(stored_api_key("anthropic"), None);
        assert_eq!(stored_api_key("groq"), None);
        write(serde_json::json!({
            "anthropic": {"access_token":""},
            "openai": {"access_token":"expired", "expires_at":"2000-01-01T00:00:00Z"},
        }));
        assert_eq!(crate::provider_from_auth_store(), None);
        assert_eq!(stored_api_key("anthropic"), None);
        assert_eq!(stored_api_key("openai"), None);
        write(serde_json::json!({
            "anthropic": {"access_token":"preferred"},
            "openai": {"access_token":"other"},
        }));
        assert_eq!(crate::provider_from_auth_store().as_deref(), Some("anthropic"));
        assert_eq!(stored_api_key("anthropic").as_deref(), Some("preferred"));
    }
}
