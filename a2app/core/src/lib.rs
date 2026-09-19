//! Core mini-app support for Robrix: the launcher-independent half of
//! hosting sandboxed Splash mini-apps, ported from `host_launcher`.
//!
//! The host app (Robrix) injects a data root via [`set_data_root`] before
//! using anything else; all on-disk state lives under it:
//! `apps/<id>/` holds manifests, source and versions. Active filesystem jails
//! live in `app_compartments/<context-hash>/`, separated by account, app,
//! room and public/private context. `app_data/<id>/` is a retained legacy
//! archive, never automatically mounted in a new context. Host-owned
//! `information_flow.json` keeps provenance and permanent sharing rules
//! outside those jails; `permissions.json`, `a2app_state.json` and `exchange/`
//! hold the remaining grants, registry state and exchange files.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The manifest header comment format (`// name:` etc.) shared by the
/// generation pipeline and bare-source imports.
pub mod header;
/// Mini-app manifests, scopes, and the in-memory registry.
pub mod manifest;
/// Where docked panes sit, per (app, room).
pub mod layout;
/// The `.splashapp` bundle format for exporting/importing/sharing apps.
pub mod bundle;
/// Version history snapshots for modified apps.
pub mod versions;
/// Line diffs between two versions of an app's source.
pub mod diff;
/// The permission model: declarations, grants, prompts, restrictions.
pub mod permissions;
/// Host-owned information-flow labels and source-to-recipient sharing rules.
pub mod information_flow;
/// The tagged catalog of every ability, layered over the permission groups.
pub mod capabilities;
/// Built-in sample apps.
pub mod builtin;
/// On-disk persistence for apps, grants, and registry state.
pub mod persistence;
/// The host-service broker that answers `host.request(...)` calls from isolates.
pub mod services;

static DATA_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Sets the directory that all a2app state lives under. Call once at startup;
/// later calls are ignored.
pub fn set_data_root(root: PathBuf) {
    let _ = DATA_ROOT.set(root);
}

/// The a2app data root. Falls back to a per-process temp dir if the host
/// never set one, so tests can't touch a real profile.
pub fn data_root() -> &'static Path {
    DATA_ROOT.get_or_init(|| {
        std::env::temp_dir().join(format!("robrix_a2app_{}", std::process::id()))
    })
}

/// The retained legacy shared jail. New instances use the IFC registry's
/// `context_storage_path`, partitioned by account, app and room.
pub fn app_sandbox_dir(app_id: &str) -> PathBuf {
    data_root().join("app_data").join(app_id)
}
