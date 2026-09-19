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
    #[cfg(feature = "embedded")]
    { embedded::resolve(prefs).map(|config| config.recipient) }
    #[cfg(not(feature = "embedded"))]
    {
        let _ = prefs;
        Err("Private-data sharing requires the embedded model backend.".into())
    }
}

#[cfg(feature = "embedded")]
mod embedded;
#[cfg(feature = "embedded")]
pub(crate) use embedded::provider;
