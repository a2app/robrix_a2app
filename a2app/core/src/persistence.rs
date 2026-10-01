//! Saving and restoring mini-app state, modularly — one massive JSON would be
//! fragile (a single corrupt byte loses everything, and every app's source
//! code would ride along with every small change). The layout:
//!
//! ```text
//! <data_root>/
//!   a2app_state.json          archived apps, recents
//!   permissions.json         persistent capability grants and room/space rules
//!   information_flow.json    retained provenance and permanent sharing rules
//!   apps/<id>/manifest.json   one user/generated app's metadata
//!   apps/<id>/app.splash      ...its source code, a real editable file
//!   apps/<id>/widget.splash   ...its widget's source, if a bundle carried one
//!   apps/<id>/versions/       ...every version it has been, timestamped
//!   app_compartments/<hash>/  separate account/app/room or public-context jail
//!   app_data/<id>/            retained legacy shared storage, not an active jail
//! ```
//!
//! A compartment hash identifies the complete context, including public/private
//! mode. Legacy files remain available for storage accounting and explicit
//! deletion, but are never automatically mounted in a new compartment. Clearing
//! data removes compartment and legacy files while retaining host provenance.
//!
//! Stock built-in apps live in the binary/repo (`apps/*.splash`); user-modified
//! built-in overrides may also have manifests and source in this data root.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use makepad_widgets::error;
use serde::{Deserialize, Serialize};

use crate::{
    data_root,
    manifest::{A2AppScope, MiniAppId, MiniAppManifest, WidgetManifest},
    versions::{AppVersion, VersionOrigin, VersionSnapshot, new_version},
};

const PERMISSIONS_FILE_NAME: &str = "permissions.json";
const STATE_FILE_NAME: &str = "a2app_state.json";

/// Persists the user's permission grants. Its own file: a corrupt byte here
/// must cost re-asking a few prompts, never the rest of the state.
pub fn save_permissions(store: &crate::permissions::PermissionStore) -> Result<()> {
    std::fs::create_dir_all(data_root())?;
    atomic_write(
        &data_root().join(PERMISSIONS_FILE_NAME),
        &serde_json::to_vec_pretty(store)?,
    )?;
    Ok(())
}

/// Saved grants, or the default store on a first run or unreadable file.
/// The default global write policy blocks room writes.
pub fn load_permissions() -> crate::permissions::PermissionStore {
    let mut store: crate::permissions::PermissionStore = std::fs::read(data_root().join(PERMISSIONS_FILE_NAME))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    // A store written by an older build may carry grants whose meaning has
    // changed (see `PermissionStore::migrate`).
    store.migrate();
    store
}

/// Registry state that isn't an app of its own: uninstall archives and
/// last-opened times.
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct A2AppPersistedState {
    /// Manifests of uninstalled apps the user made or imported. A generated
    /// app exists nowhere else: uninstalling it used to destroy the only
    /// copy, and a prompt you can't reproduce is real work gone. Keeping the
    /// manifest costs a few KB and makes uninstall reversible.
    #[serde(default)]
    pub archived: Vec<MiniAppManifest>,
    /// Complete portable histories retained before an uninstall removes code.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub archived_bundles: BTreeMap<MiniAppId, String>,
    /// Latest shipped default, independent of the user's current branch.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub builtin_baselines: BTreeMap<MiniAppId, crate::builtin::BuiltinBaseline>,
    /// Updated default snapshots offered to users who customized an app.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub builtin_updates: BTreeMap<MiniAppId, String>,
    /// Unix timestamp (secs) of when each app was last opened, for "recents".
    #[serde(default)]
    pub recents: BTreeMap<MiniAppId, u64>,
}

pub fn save_registry_state(state: &A2AppPersistedState) -> Result<()> {
    std::fs::create_dir_all(data_root())?;
    atomic_write(
        &data_root().join(STATE_FILE_NAME),
        &serde_json::to_vec_pretty(state)?,
    )?;
    Ok(())
}

/// Saved registry state, or the empty default on a first run/unreadable file.
pub fn load_registry_state() -> A2AppPersistedState {
    std::fs::read(data_root().join(STATE_FILE_NAME))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn apps_dir() -> PathBuf {
    data_root().join("apps")
}

/// Whether an app id is safe to use as a single directory name. Generated ids
/// are kebab-case, but an imported file could carry anything — reject path
/// separators, `..`, absolute markers, and NUL so an id can never point a
/// write outside `apps/`/`app_data/`. Defense in depth; the id sources are all
/// trusted today.
fn is_safe_app_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && !id.contains(['/', '\\', '\0'])
        && !std::path::Path::new(id)
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
}

fn app_dir(id: &str) -> PathBuf {
    apps_dir().join(id)
}

/// Whether an installed or interrupted import already owns this directory.
pub fn has_app_files(id: &str) -> bool {
    is_safe_app_id(id) && app_dir(id).exists()
}

/// Writes `bytes` to `path` atomically: a sibling temp file renamed over the
/// target (same-dir rename is atomic on the platforms we target). A crash
/// mid-write leaves the OLD file intact rather than a truncated one — the
/// whole point of splitting the store into per-file pieces.
pub fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// The on-disk manifest: everything about an app EXCEPT its id (the directory
/// name) and its sources (their own files beside it).
#[derive(Serialize, Deserialize)]
struct AppManifestFile {
    name: String,
    icon: String,
    tint: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default)]
    allow_net: bool,
    /// Declared permission ids. Declarations only — the user's grants live in
    /// the host's own permissions.json.
    #[serde(default)]
    permissions: Vec<String>,
    /// The app's own reason per permission, shown on the prompt.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    permission_reasons: std::collections::BTreeMap<String, String>,
    /// Capability ids narrowing the declared groups.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    capabilities: Vec<String>,
    /// True for a user-modified copy of a BUILT-IN app: the override shadows
    /// the stock manifest at load, and keeping the flag means it stays
    /// non-uninstallable (you revert it via version history instead).
    #[serde(default)]
    builtin: bool,
    #[serde(default)]
    shortcuts: Vec<String>,
    /// Account app or attached to one room.
    #[serde(default)]
    scope: A2AppScope,
    /// Spans for the widget whose source is `widget.splash`, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    widget: Option<WidgetSpans>,
    #[serde(default)]
    current_version: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct WidgetSpans {
    default_span: (u8, u8),
    min_span: (u8, u8),
}

/// Writes one user app's directory: manifest + source file(s). Called on
/// install/refine — NOT on every open, so touching an app never rewrites
/// its code.
pub fn save_user_app(manifest: &MiniAppManifest) -> Result<()> {
    if !is_safe_app_id(&manifest.id) {
        anyhow::bail!("refusing to persist app with unsafe id '{}'", manifest.id);
    }
    let dir = app_dir(&manifest.id);
    std::fs::create_dir_all(&dir)?;
    let file = AppManifestFile {
        name: manifest.name.clone(),
        icon: manifest.icon.clone(),
        tint: manifest.tint,
        description: Some(manifest.description.clone()),
        allow_net: manifest.allow_net,
        permissions: manifest.permissions.clone(),
        permission_reasons: manifest.permission_reasons.clone(),
        capabilities: manifest.capabilities.clone(),
        builtin: manifest.builtin,
        shortcuts: manifest.shortcuts.clone(),
        scope: manifest.scope.clone(),
        widget: manifest.widget.as_ref().map(|w| WidgetSpans {
            default_span: w.default_span,
            min_span: w.min_span,
        }),
        current_version: manifest.current_version.clone(),
    };
    // Order matters for crash safety: write the SOURCES first, and
    // manifest.json (the file load keys off) LAST, each atomically. A crash
    // between them leaves either the old complete app or the new complete app
    // — never a manifest pointing at a half-written source.
    atomic_write(&dir.join("app.splash"), manifest.source.as_bytes())?;
    match &manifest.widget {
        Some(w) => atomic_write(&dir.join("widget.splash"), w.source.as_bytes())?,
        None => {
            let _ = std::fs::remove_file(dir.join("widget.splash"));
        }
    }
    atomic_write(&dir.join("manifest.json"), &serde_json::to_vec_pretty(&file)?)?;
    Ok(())
}

fn versions_dir(id: &str) -> PathBuf {
    app_dir(id).join("versions")
}

/// `versions/<stamp>.<ext>`, or None for an id/stamp that could leave the dir.
fn version_file(id: &str, stamp: &str, ext: &str) -> Option<PathBuf> {
    (is_safe_app_id(id) && is_safe_app_id(stamp))
        .then(|| versions_dir(id).join(format!("{stamp}.{ext}")))
}

/// Appends `version` (with `manifest.source`) to the app's history, returning
/// its final stamp: a same-second collision gets a `-2`, `-3`... suffix.
pub fn append_version(manifest: &MiniAppManifest, mut version: AppVersion) -> Result<String> {
    if !is_safe_app_id(&manifest.id) {
        anyhow::bail!("refusing to version app with unsafe id '{}'", manifest.id);
    }
    if !is_safe_app_id(&version.stamp) {
        anyhow::bail!("refusing to persist an unsafe version stamp");
    }
    let dir = versions_dir(&manifest.id);
    std::fs::create_dir_all(&dir)?;

    let base = version.stamp.clone();
    let mut n = 2;
    while dir.join(format!("{}.json", version.stamp)).exists() {
        version.stamp = format!("{base}-{n}");
        n += 1;
        if n > 60 {
            anyhow::bail!("too many versions in the same second");
        }
    }

    // Source first, metadata (the file listing keys off) last.
    atomic_write(
        &dir.join(format!("{}.splash", version.stamp)),
        manifest.source.as_bytes(),
    )?;
    atomic_write(
        &dir.join(format!("{}.json", version.stamp)),
        &serde_json::to_vec_pretty(&version)?,
    )?;
    Ok(version.stamp)
}

/// Compat shim; see `append_version`.
pub fn snapshot_version(manifest: &MiniAppManifest, version: AppVersion) -> Result<()> {
    append_version(manifest, version).map(|_| ())
}

/// Every version of an app, oldest first.
pub fn list_versions(id: &str) -> Vec<AppVersion> {
    if !is_safe_app_id(id) {
        return Vec::new();
    }
    let Ok(read) = std::fs::read_dir(versions_dir(id)) else {
        return Vec::new();
    };
    let mut out: Vec<AppVersion> = read
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read(e.path()).ok())
        .filter_map(|bytes| serde_json::from_slice::<AppVersion>(&bytes).ok())
        // A metadata file whose source went missing is not restorable.
        .filter(|v| version_file(id, &v.stamp, "splash").is_some_and(|p| p.exists()))
        .collect();
    // Same-second versions carry a "-2", "-3"... suffix, which sorts wrong as
    // text ("-10" < "-2"), so compare that index numerically.
    out.sort_by(|a, b| {
        a.at_unix
            .cmp(&b.at_unix)
            .then(collision_index(&a.stamp).cmp(&collision_index(&b.stamp)))
    });
    out
}

/// The `-N` suffix a same-second version carries (0 when there is none).
fn collision_index(stamp: &str) -> u32 {
    // Stamps look like `20260727-105412` or `20260727-105412-3`; only the
    // THIRD segment is a collision counter.
    stamp
        .splitn(3, '-')
        .nth(2)
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// One version's record and its archived source.
pub fn load_version(id: &str, stamp: &str) -> Option<(AppVersion, String)> {
    let bytes = std::fs::read(version_file(id, stamp, "json")?).ok()?;
    let version: AppVersion = serde_json::from_slice(&bytes).ok()?;
    if version.stamp != stamp { return None; }
    let source = std::fs::read_to_string(version_file(id, stamp, "splash")?).ok()?;
    Some((version, source))
}

/// Compat shim; see `load_version`.
pub fn load_version_source(id: &str, stamp: &str) -> Option<String> {
    load_version(id, stamp).map(|(_, source)| source)
}

/// Loads every historical source for export, reporting damage instead of
/// silently dropping an unreadable record from the shared provenance.
pub fn export_history(id: &str) -> Result<Vec<VersionSnapshot>> {
    if !is_safe_app_id(id) { anyhow::bail!("unsafe app identity"); }
    let entries = match std::fs::read_dir(versions_dir(id)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut history = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if !path.extension().is_some_and(|ext| ext == "json") { continue; }
        let version: AppVersion = serde_json::from_slice(&std::fs::read(&path)?)?;
        if path.file_stem().and_then(|stem| stem.to_str()) != Some(version.stamp.as_str()) {
            anyhow::bail!("a history file does not match its version stamp");
        }
        let source_path = version_file(id, &version.stamp, "splash")
            .ok_or_else(|| anyhow::anyhow!("unsafe history stamp"))?;
        history.push(VersionSnapshot { version, source: std::fs::read_to_string(source_path)? });
    }
    validate_history(&history, None)?;
    history.sort_by(|a, b| a.version.at_unix.cmp(&b.version.at_unix)
        .then(collision_index(&a.version.stamp).cmp(&collision_index(&b.version.stamp))));
    Ok(history)
}

/// Checks portable history before any part of it is written to disk.
pub fn validate_history(history: &[VersionSnapshot], current_version: Option<&str>) -> Result<()> {
    let mut stamps = std::collections::BTreeMap::new();
    for snapshot in history {
        let version = &snapshot.version;
        if !is_safe_app_id(&version.stamp) || version.stamp.len() > 96
            || stamps.insert(version.stamp.as_str(), version).is_some() {
            anyhow::bail!("history contains an unsafe or duplicate version stamp");
        }
        if snapshot.source.trim().is_empty() {
            anyhow::bail!("history contains a version without source code");
        }
    }
    let mut checked = std::collections::BTreeSet::new();
    for snapshot in history {
        let mut ancestor = snapshot.version.parent.as_deref();
        let mut visited = std::collections::BTreeSet::from([snapshot.version.stamp.as_str()]);
        while let Some(stamp) = ancestor {
            let Some(parent) = stamps.get(stamp) else {
                anyhow::bail!("history refers to a missing parent version");
            };
            if !visited.insert(stamp) {
                anyhow::bail!("history contains a cycle");
            }
            if checked.contains(stamp) { break; }
            ancestor = parent.parent.as_deref();
        }
        checked.extend(visited);
    }
    if current_version.is_some_and(|stamp| !stamps.contains_key(stamp)) {
        anyhow::bail!("history refers to a missing current version");
    }
    Ok(())
}

/// Restores all historical code and metadata into a newly assigned app id.
///
/// Stamps and parent links stay intact. Existing history is never overwritten,
/// and shared attribution is explicitly marked as unverified.
pub fn import_history(
    manifest: &mut MiniAppManifest,
    history: &[VersionSnapshot],
    current_version: Option<&str>,
) -> Result<()> {
    if !is_safe_app_id(&manifest.id) {
        anyhow::bail!("refusing to import history for an unsafe app id");
    }
    validate_history(history, current_version)?;
    if let Some(stamp) = current_version {
        let current = history.iter().find(|entry| entry.version.stamp == stamp).unwrap();
        if current.source != manifest.source {
            anyhow::bail!("the current history version does not match the app source");
        }
    }
    let dir = versions_dir(&manifest.id);
    for snapshot in history {
        if dir.join(format!("{}.json", snapshot.version.stamp)).exists()
            || dir.join(format!("{}.splash", snapshot.version.stamp)).exists() {
            anyhow::bail!("the destination already contains this version history");
        }
    }
    if !history.is_empty() {
        std::fs::create_dir_all(&dir)?;
    }
    for snapshot in history {
        let mut version = snapshot.version.clone();
        version.imported = true;
        atomic_write(&dir.join(format!("{}.splash", version.stamp)), snapshot.source.as_bytes())?;
        atomic_write(&dir.join(format!("{}.json", version.stamp)), &serde_json::to_vec_pretty(&version)?)?;
    }
    manifest.current_version = current_version.map(str::to_string);
    Ok(())
}

/// Saves the working copy when its recorded version no longer matches it.
///
/// An external source edit or upgraded metadata keeps the previous snapshot
/// as its parent; a missing snapshot never becomes a dangling parent link.
pub fn ensure_current_version(
    manifest: &mut MiniAppManifest,
    origin: VersionOrigin,
    note: &str,
    at_unix: u64,
    offset_secs: i64,
) -> Result<()> {
    let previous = manifest.current_version.as_deref()
        .and_then(|stamp| load_version(&manifest.id, stamp))
        .map(|(version, source)| VersionSnapshot { version, source });
    if previous.as_ref().is_some_and(|snapshot| snapshot.matches_manifest(manifest)) {
        return Ok(());
    }
    let parent = previous.as_ref().map(|snapshot| snapshot.version.stamp.as_str());
    let version = new_version(manifest, origin, note, parent, at_unix, offset_secs);
    manifest.current_version = Some(append_version(manifest, version)?);
    save_user_app(manifest)
}

/// Bytes in an app's isolated compartments and retained legacy storage.
pub fn app_data_bytes(id: &str) -> u64 {
    fn walk(dir: &std::path::Path) -> u64 {
        let Ok(read) = std::fs::read_dir(dir) else {
            return 0;
        };
        read.flatten()
            .map(|e| match std::fs::symlink_metadata(e.path()) {
                Ok(m) if m.file_type().is_symlink() => 0,
                Ok(m) if m.is_dir() => walk(&e.path()),
                Ok(m) => m.len(),
                Err(_) => 0,
            })
            .sum()
    }
    if !is_safe_app_id(id) {
        return 0;
    }
    crate::information_flow::app_storage_paths(id)
        .unwrap_or_else(|_| vec![crate::app_sandbox_dir(id)])
        .iter().map(|path| walk(path)).sum()
}

/// Empties an app's compartment and retained legacy storage, keeping the app
/// installed and its information-flow provenance intact.
pub fn clear_app_data(id: &str) -> Result<(), String> {
    if !is_safe_app_id(id) {
        return Err("Invalid app identity.".into());
    }
    for dir in crate::information_flow::app_storage_paths(id)? {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("Couldn't clear app storage: {error}")),
        }
    }
    Ok(())
}

/// Removes an uninstalled app's code directory and retained legacy data.
/// This does not remove compartment storage or information-flow provenance.
pub fn remove_user_app(id: &str) {
    if !is_safe_app_id(id) {
        return;
    }
    let _ = std::fs::remove_dir_all(app_dir(id));
    let _ = std::fs::remove_dir_all(crate::app_sandbox_dir(id));
}

/// Reads one app directory back into a manifest. Skips (with a log) rather
/// than fails: one broken app must not take the host down.
fn load_user_app(id: &str) -> Option<MiniAppManifest> {
    let dir = app_dir(id);
    let manifest_bytes = std::fs::read(dir.join("manifest.json")).ok()?;
    let file: AppManifestFile = match serde_json::from_slice(&manifest_bytes) {
        Ok(f) => f,
        Err(e) => {
            error!("Skipping app '{id}': bad manifest.json: {e}");
            return None;
        }
    };
    let source = match std::fs::read_to_string(dir.join("app.splash")) {
        Ok(s) => s,
        Err(e) => {
            error!("Skipping app '{id}': missing app.splash: {e}");
            return None;
        }
    };
    let widget = match file.widget {
        Some(spans) => match std::fs::read_to_string(dir.join("widget.splash")) {
            Ok(widget_source) => Some(WidgetManifest {
                source: widget_source,
                default_span: spans.default_span,
                min_span: spans.min_span,
            }),
            Err(_) => {
                error!("App '{id}': manifest promises a widget but widget.splash is missing");
                None
            }
        },
        None => None,
    };
    let mut manifest = MiniAppManifest {
        id: id.to_string(),
        name: file.name,
        icon: file.icon,
        tint: file.tint,
        // Copies saved before descriptions existed still have the header line.
        description: file.description.unwrap_or_else(||
            crate::header::parse_app_header(&source).description.unwrap_or_default()),
        source,
        allow_net: file.allow_net,
        permissions: file.permissions,
        permission_reasons: file.permission_reasons,
        capabilities: file.capabilities,
        builtin: file.builtin,
        widget,
        shortcuts: file.shortcuts,
        scope: file.scope,
        current_version: file.current_version,
    };
    // A saved copy of a built-in shadows the stock app, so it must keep what
    // the stock app declares.
    crate::builtin::union_stock_declarations(&mut manifest);
    manifest.normalize_permissions();
    Some(manifest)
}

/// Every user app on disk, by scanning `apps/*/`. Public so init can recover
/// apps even when other state files are missing/corrupt (the code outlives
/// them).
pub fn load_user_apps() -> Vec<MiniAppManifest> {
    let mut apps = Vec::new();
    let Ok(read) = std::fs::read_dir(apps_dir()) else {
        return apps;
    };
    let mut ids: Vec<String> = read
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|id| is_safe_app_id(id))
        .collect();
    ids.sort();
    for id in ids {
        if let Some(app) = load_user_app(&id) {
            apps.push(app);
        }
    }
    apps
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A built-in modified before one of its declarations existed must not
    /// stay stripped of it. Its saved copy shadows the code, and a built-in's
    /// declarations are not user-editable, so an empty list on disk is a lost
    /// capability with no way back.
    #[test]
    fn a_saved_builtin_keeps_what_the_builtin_declares() {
        // data_root() falls back to a per-process temp dir in tests, so this
        // never touches a real profile.
        let dir = app_dir("room-peek");
        std::fs::create_dir_all(&dir).unwrap();
        // Exactly what a pre-permissions save looks like: no `permissions`
        // key at all.
        std::fs::write(
            dir.join("manifest.json"),
            br#"{"name":"Room Peek","icon":"P","tint":123,"allow_net":false,"builtin":true,"shortcuts":[]}"#,
        )
        .unwrap();
        std::fs::write(dir.join("app.splash"), b"View{}").unwrap();

        let m = load_user_app("room-peek").expect("loads");
        assert!(
            m.declares(crate::permissions::Permission::MatrixRoomInfo),
            "a saved built-in keeps the declarations it carries in code"
        );
        assert!(
            m.declares(crate::permissions::Permission::MatrixRoomSend),
            "and the rest of its declarations with it"
        );
        assert!(
            m.reason_for(crate::permissions::Permission::MatrixRoomRead).is_some(),
            "with the stock reason, so the prompt still explains itself"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A user's own app is NOT topped up: an app that declares nothing is
    /// entitled to declare nothing.
    #[test]
    fn a_users_own_app_is_left_alone() {
        let dir = app_dir("peek-lookalike");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            br#"{"name":"Mine","icon":"x","tint":1,"allow_net":false,"builtin":false,"shortcuts":[]}"#,
        )
        .unwrap();
        std::fs::write(dir.join("app.splash"), b"View{}").unwrap();
        let m = load_user_app("peek-lookalike").expect("loads");
        assert!(m.permissions.is_empty(), "nothing is added to a user's own app");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An app saved, loaded back, and removed keeps its identity and scope.
    #[test]
    fn a_user_app_round_trips_through_its_directory() {
        let m = MiniAppManifest {
            id: "round-trip".into(),
            name: "Round Trip".into(),
            icon: "🧪".into(),
            tint: 0x123456,
            description: String::new(),
            source: "View{}".into(),
            allow_net: false,
            permissions: vec![],
            permission_reasons: Default::default(),
            capabilities: Vec::new(),
            builtin: false,
            widget: None,
            shortcuts: vec![],
            scope: A2AppScope::Room { room_id: "!r:example.org".into() },
            current_version: Some("20260724-153204".into()),
        };
        save_user_app(&m).unwrap();
        let loaded = load_user_app("round-trip").expect("loads");
        assert_eq!(loaded.name, "Round Trip");
        assert_eq!(loaded.source, "View{}");
        assert_eq!(loaded.scope, A2AppScope::Room { room_id: "!r:example.org".into() });
        assert_eq!(loaded.current_version.as_deref(), Some("20260724-153204"));

        // Uninstall removes code AND data dirs.
        std::fs::create_dir_all(crate::app_sandbox_dir("round-trip")).unwrap();
        remove_user_app("round-trip");
        assert!(!app_dir("round-trip").exists());
        assert!(!crate::app_sandbox_dir("round-trip").exists());
    }

    #[test]
    fn explicit_empty_descriptions_survive_disk_load_beside_a_source_header() {
        let mut m = manifest("hist-empty-description");
        m.description.clear();
        m.source = "// description: Header description\nView{}".into();
        save_user_app(&m).unwrap();
        let loaded = load_user_app(&m.id).unwrap();
        assert!(loaded.description.is_empty());
        assert!(crate::builtin::matches_default(&m, &loaded));
        let mut older = new_version(&m, VersionOrigin::Stock, "Older snapshot", None, T, 0);
        older.description = None;
        let older_snapshot = VersionSnapshot { version: older, source: m.source.clone() };
        assert!(!older_snapshot.matches_manifest(&loaded));
        let mut header_copy = loaded.clone();
        header_copy.description = "Header description".into();
        assert!(older_snapshot.matches_manifest(&header_copy));
        remove_user_app(&m.id);
    }

    #[test]
    fn collision_index_orders_same_second_snapshots() {
        // Plain stamps have no counter; suffixed ones sort numerically, so a
        // 10th snapshot in one second still comes after the 2nd.
        assert_eq!(collision_index("20260727-105412"), 0);
        assert_eq!(collision_index("20260727-105412-2"), 2);
        assert_eq!(collision_index("20260727-105412-10"), 10);
        assert!(collision_index("20260727-105412-10") > collision_index("20260727-105412-2"));
    }

    fn manifest(id: &str) -> MiniAppManifest {
        MiniAppManifest {
            id: id.into(),
            name: "Hist".into(),
            icon: "h".into(),
            tint: 1,
            description: String::new(),
            source: "View{ v1 }".into(),
            allow_net: false,
            permissions: vec!["location".into()],
            permission_reasons: [("location".to_string(), "Uses your city.".to_string())].into(),
            capabilities: Vec::new(),
            builtin: false,
            widget: None,
            shortcuts: vec![],
            scope: Default::default(),
            current_version: None,
        }
    }

    // 2026-07-24 15:32:04 UTC.
    const T: u64 = 1_784_907_124;

    #[test]
    fn imported_history_is_validated_before_any_files_are_written() {
        let mut m = manifest("hist-invalid-import");
        let mut version = new_version(&m, VersionOrigin::Manual, "", None, T, 0);
        let safe_stamp = version.stamp.clone();
        version.stamp = "../../escape".into();
        let mut history = vec![VersionSnapshot { version, source: m.source.clone() }];
        assert!(import_history(&mut m, &history, None).is_err());
        assert!(!app_dir(&m.id).exists());
        history[0].version.stamp = safe_stamp.clone();
        history[0].version.parent = Some("absent".into());
        assert!(import_history(&mut m, &history, None).is_err());
        assert!(!app_dir(&m.id).exists());
        history[0].version.parent = Some(safe_stamp.clone());
        assert!(import_history(&mut m, &history, None).is_err());
        assert!(!app_dir(&m.id).exists());
        history[0].version.parent = None;
        history.push(history[0].clone());
        assert!(import_history(&mut m, &history, None).is_err());
        history.pop();
        assert!(import_history(&mut m, &history, Some("absent")).is_err());
        history[0].source = "Different{}".into();
        assert!(import_history(&mut m, &history, Some(&safe_stamp)).is_err());
        assert!(!app_dir(&m.id).exists());
    }

    #[test]
    fn importing_history_never_overwrites_an_existing_version() {
        let mut m = manifest("hist-import-no-overwrite");
        let version = new_version(&m, VersionOrigin::Manual, "", None, T, 0);
        let stamp = append_version(&m, version.clone()).unwrap();
        let history = vec![VersionSnapshot { version, source: "Replacement{}".into() }];
        assert!(import_history(&mut m, &history, None).is_err());
        assert_eq!(load_version(&m.id, &stamp).unwrap().1, m.source);
        remove_user_app(&m.id);
    }

    #[test]
    fn history_parent_cycles_are_rejected_in_any_input_order() {
        let m = manifest("hist-cycle");
        let mut first = new_version(&m, VersionOrigin::Manual, "", None, T, 0);
        let second = new_version(&m, VersionOrigin::Manual, "", Some(&first.stamp), T + 1, 0);
        first.parent = Some(second.stamp.clone());
        let history = vec![
            VersionSnapshot { version: second, source: m.source.clone() },
            VersionSnapshot { version: first, source: m.source.clone() },
        ];
        assert!(validate_history(&history, None).unwrap_err().to_string().contains("cycle"));
    }

    #[test]
    fn old_version_files_preserve_widgets_that_were_not_recorded() {
        let mut m = manifest("hist-legacy-widget");
        m.widget = Some(WidgetManifest { source: "OldWidget{}".into(), default_span: (2, 2), min_span: (1, 1) });
        let old: AppVersion = serde_json::from_str(
            r#"{"stamp":"20260724-153204","at_unix":1784907124,"note":"x","name":"N","icon":"i","tint":7,"origin":"Manual"}"#,
        ).unwrap();
        assert!(!old.widget_recorded);
        assert_eq!(old.apply_to(&m, m.source.clone()).widget, m.widget);
        let mut current = new_version(&m, VersionOrigin::Manual, "", None, T, 0);
        current.widget = None;
        assert!(current.apply_to(&m, m.source.clone()).widget.is_none());
    }

    #[test]
    fn archive_state_round_trips_complete_histories_and_loads_old_files() {
        let old: A2AppPersistedState = serde_json::from_str(r#"{"archived":[],"recents":{}}"#).unwrap();
        assert!(old.archived_bundles.is_empty());
        let m = manifest("hist-archive-state");
        let mut state = A2AppPersistedState::default();
        state.archived.push(m.clone());
        state.archived_bundles.insert(m.id.clone(), crate::bundle::try_to_text(&m).unwrap());
        let restored: A2AppPersistedState = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        let bundle = crate::bundle::parse_with_history(&restored.archived_bundles[&m.id]).unwrap();
        assert_eq!(bundle.history.len(), 1);
        assert_eq!(bundle.history[0].source, m.source);
    }

    #[test]
    fn a_version_round_trips_through_append_list_and_load() {
        let m = manifest("hist-round-trip");
        let version = new_version(&m, VersionOrigin::Ai, "  make it blue ", Some("20260724-150000"), T, 0);
        let stamp = append_version(&m, version).unwrap();
        assert_eq!(stamp, "20260724-153204");

        let listed = list_versions("hist-round-trip");
        assert_eq!(listed.len(), 1);
        let v = &listed[0];
        assert_eq!(v.stamp, stamp);
        assert_eq!(v.origin, VersionOrigin::Ai);
        assert_eq!(v.note, "make it blue");
        assert_eq!(v.parent.as_deref(), Some("20260724-150000"));
        assert_eq!(v.permissions, m.permissions);
        assert_eq!(v.permission_reasons, m.permission_reasons);

        let (v, source) = load_version("hist-round-trip", &stamp).expect("loads");
        assert_eq!(v.stamp, stamp);
        assert_eq!(source, "View{ v1 }");
        assert!(load_version("hist-round-trip", "20260101-000000").is_none());
        assert!(load_version("hist-round-trip", "../escape").is_none());
        let _ = std::fs::remove_dir_all(app_dir("hist-round-trip"));
    }

    #[test]
    fn versions_list_oldest_first_across_same_second_collisions() {
        let m = manifest("hist-order");
        // Written newest first, so the order has to come from the records.
        let last = append_version(&m, new_version(&m, VersionOrigin::Manual, "", None, T + 60, 0)).unwrap();
        let mut expected = vec![append_version(&m, new_version(&m, VersionOrigin::Manual, "", None, T, 0)).unwrap()];
        for _ in 0..10 {
            expected.push(append_version(&m, new_version(&m, VersionOrigin::Manual, "", None, T, 0)).unwrap());
        }
        expected.push(last);
        assert_eq!(expected[1], "20260724-153204-2");
        assert_eq!(expected[10], "20260724-153204-11");

        let stamps: Vec<String> = list_versions("hist-order").into_iter().map(|v| v.stamp).collect();
        assert_eq!(stamps, expected);
        let _ = std::fs::remove_dir_all(app_dir("hist-order"));
    }

    #[test]
    fn ensure_current_version_appends_once_and_saves_the_pointer() {
        let mut m = manifest("hist-ensure");
        ensure_current_version(&mut m, VersionOrigin::Import, "Original", T, 0).unwrap();
        let stamp = m.current_version.clone().expect("pointer set");
        assert_eq!(list_versions("hist-ensure").len(), 1);
        assert_eq!(list_versions("hist-ensure")[0].origin, VersionOrigin::Import);
        let saved = load_user_app("hist-ensure").expect("saved");
        assert_eq!(saved.current_version.as_deref(), Some(stamp.as_str()));

        // Already a version on disk: nothing appended, pointer untouched.
        ensure_current_version(&mut m, VersionOrigin::Ai, "again", T + 5, 0).unwrap();
        assert_eq!(m.current_version.as_deref(), Some(stamp.as_str()));
        assert_eq!(list_versions("hist-ensure").len(), 1);

        // A pointer at a version that's gone is replaced by a fresh one.
        m.current_version = Some("19990101-000000".into());
        ensure_current_version(&mut m, VersionOrigin::Ai, "again", T + 5, 0).unwrap();
        assert_eq!(m.current_version.as_deref(), Some("20260724-153209"));
        assert_eq!(list_versions("hist-ensure").len(), 2);
        let _ = std::fs::remove_dir_all(app_dir("hist-ensure"));
    }

    #[test]
    fn an_external_source_edit_keeps_the_preceding_code_and_unknown_author() {
        let mut m = manifest("hist-external-edit");
        ensure_current_version(&mut m, VersionOrigin::Ai, "Generated", T, 0).unwrap();
        let original = m.current_version.clone().unwrap();
        std::fs::write(app_dir(&m.id).join("app.splash"), b"View{ external_edit }").unwrap();
        let mut edited = load_user_app(&m.id).unwrap();
        ensure_current_version(&mut edited, VersionOrigin::Legacy, "Observed saved copy", 0, 0).unwrap();
        let history = export_history(&m.id).unwrap();
        assert_eq!(history.len(), 2);
        let baseline = load_version(&m.id, edited.current_version.as_deref().unwrap()).unwrap();
        assert_eq!(baseline.0.parent.as_deref(), Some(original.as_str()));
        assert_eq!(baseline.0.at_unix, 0);
        assert!(baseline.0.actor.is_none());
        assert_eq!(baseline.1, "View{ external_edit }");
        assert_eq!(load_version(&m.id, &original).unwrap().1, m.source);
        assert!(has_app_files(&m.id));
        remove_user_app(&m.id);
        assert!(!has_app_files(&m.id));
        assert!(!has_app_files("../escape"));
    }

    #[test]
    fn upgraded_metadata_gets_a_snapshot_without_losing_the_previous_contract() {
        let mut m = manifest("hist-upgraded-metadata");
        ensure_current_version(&mut m, VersionOrigin::Ai, "Generated", T, 0).unwrap();
        let original = m.current_version.clone().unwrap();
        m.permissions.push("network".into());
        m.normalize_permissions();
        ensure_current_version(&mut m, VersionOrigin::Legacy, "Observed saved copy", 0, 0).unwrap();
        assert_eq!(list_versions(&m.id).len(), 2);
        let latest = load_version(&m.id, m.current_version.as_deref().unwrap()).unwrap().0;
        assert_eq!(latest.parent.as_deref(), Some(original.as_str()));
        assert_eq!(latest.permissions, m.permissions);
        assert_eq!(load_version(&m.id, &original).unwrap().0.permissions, vec!["location"]);
        remove_user_app(&m.id);
    }

    #[test]
    fn apply_to_takes_declarations_only_from_full_versions() {
        let mut base = manifest("hist-apply");
        base.permissions = vec!["network".into()];
        base.permission_reasons.clear();
        base.normalize_permissions();
        let older = manifest("hist-apply");

        let full = new_version(&older, VersionOrigin::Ai, "", None, T, 0);
        let restored = full.apply_to(&base, "View{ v1 }".into());
        assert_eq!(restored.id, "hist-apply");
        assert_eq!(restored.source, "View{ v1 }");
        assert_eq!(restored.permissions, vec!["location".to_string()]);
        assert_eq!(restored.permission_reasons, older.permission_reasons);
        assert!(!restored.allow_net);
        assert_eq!(restored.current_version.as_deref(), Some("20260724-153204"));

        // An old Legacy file never recorded declarations, so base keeps its own.
        let legacy: AppVersion = serde_json::from_str(
            r#"{"stamp":"20260724-153204","at_unix":1784907124,"note":"","name":"Hist","icon":"h","tint":1}"#,
        ).unwrap();
        let restored = legacy.apply_to(&base, "View{ v1 }".into());
        assert_eq!(restored.permissions, vec!["network".to_string()]);
        assert!(restored.allow_net);
        assert_eq!(restored.current_version.as_deref(), Some("20260724-153204"));
        // A newly observed copy records its whole contract even when its
        // original author and modification date remain unknown.
        let mut observed = older.clone();
        observed.description = "Saved local description".into();
        let modern_legacy = new_version(&observed, VersionOrigin::Legacy, "Observed saved copy", None, 0, 0);
        let restored = modern_legacy.apply_to(&base, observed.source.clone());
        assert_eq!(restored.permissions, observed.permissions);
        assert_eq!(restored.permission_reasons, observed.permission_reasons);
        assert_eq!(restored.description, observed.description);
        assert!(!restored.allow_net);
    }

    #[test]
    fn apply_to_keeps_a_builtins_stock_declarations() {
        let mut base = manifest("room-peek");
        base.builtin = true;
        let mut stripped = manifest("room-peek");
        stripped.permissions.clear();
        stripped.permission_reasons.clear();

        let restored = new_version(&stripped, VersionOrigin::Ai, "", None, T, 0).apply_to(&base, "View{}".into());
        assert!(restored.builtin);
        assert!(restored.declares(crate::permissions::Permission::MatrixRoomInfo));
        assert!(restored.reason_for(crate::permissions::Permission::MatrixRoomRead).is_some());
    }

    #[test]
    fn app_id_safety_rejects_path_structure() {
        assert!(is_safe_app_id("tip-calc"));
        assert!(is_safe_app_id("Weather2"));
        for bad in ["", ".", "..", "a/b", "../x", "/etc", "a\\b", "x\0y"] {
            assert!(!is_safe_app_id(bad), "{bad:?} should be rejected");
        }
        // save refuses an unsafe id outright.
        let m = MiniAppManifest {
            id: "../escape".into(),
            name: "X".into(),
            icon: "x".into(),
            tint: 0,
            description: String::new(),
            source: "View{}".into(),
            allow_net: false,
            permissions: vec![],
            permission_reasons: Default::default(),
            capabilities: Vec::new(),
            builtin: false,
            widget: None,
            shortcuts: vec![],
            scope: Default::default(),
            current_version: None,
        };
        assert!(save_user_app(&m).is_err());
    }
}
