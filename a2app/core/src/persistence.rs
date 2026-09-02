//! Saving and restoring mini-app state, modularly — one massive JSON would be
//! fragile (a single corrupt byte loses everything, and every app's source
//! code would ride along with every small change). The layout:
//!
//! ```text
//! <data_root>/
//!   a2app_state.json          archived apps, recents
//!   permissions.json          the user's grants
//!   apps/<id>/manifest.json   one user/generated app's metadata
//!   apps/<id>/app.splash      ...its source code, a real editable file
//!   apps/<id>/widget.splash   ...its widget's source, if a bundle carried one
//!   apps/<id>/versions/       ...every version it has been, timestamped
//!   app_data/<id>/            ...its private storage (the Splash fs jail)
//! ```
//!
//! Built-in apps live in the binary/repo (`apps/*.splash`), never here.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use makepad_widgets::error;
use serde::{Deserialize, Serialize};

use crate::{
    data_root,
    manifest::{A2AppScope, MiniAppId, MiniAppManifest, WidgetManifest},
    versions::{AppVersion, VersionOrigin, new_version},
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

/// Saved grants, or the empty (all-Ask) store on a first run or unreadable file.
pub fn load_permissions() -> crate::permissions::PermissionStore {
    std::fs::read(data_root().join(PERMISSIONS_FILE_NAME))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
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
    /// Unix timestamp (secs) of when each app was last opened, for "recents".
    #[serde(default)]
    pub recents: BTreeMap<MiniAppId, u64>,
    /// Each (app, room) instance's last dock position, keyed by instance tag.
    #[serde(default)]
    pub pane_layouts: BTreeMap<String, crate::layout::PaneLayout>,
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
    let version = serde_json::from_slice(&bytes).ok()?;
    let source = std::fs::read_to_string(version_file(id, stamp, "splash")?).ok()?;
    Some((version, source))
}

/// Compat shim; see `load_version`.
pub fn load_version_source(id: &str, stamp: &str) -> Option<String> {
    load_version(id, stamp).map(|(_, source)| source)
}

/// No-op when `current_version` names a version on disk; otherwise appends the
/// working copy as one, points at it, and saves the app.
pub fn ensure_current_version(
    manifest: &mut MiniAppManifest,
    origin: VersionOrigin,
    note: &str,
    at_unix: u64,
    offset_secs: i64,
) -> Result<()> {
    let on_disk = manifest.current_version.as_deref().is_some_and(|stamp| {
        version_file(&manifest.id, stamp, "json").is_some_and(|p| p.exists())
            && version_file(&manifest.id, stamp, "splash").is_some_and(|p| p.exists())
    });
    if on_disk {
        return Ok(());
    }
    let version = new_version(manifest, origin, note, None, at_unix, offset_secs);
    manifest.current_version = Some(append_version(manifest, version)?);
    save_user_app(manifest)
}

/// Bytes an app has stored in its private jail (`app_data/<id>/`), for the
/// app info storage line.
pub fn app_data_bytes(id: &str) -> u64 {
    fn walk(dir: &std::path::Path) -> u64 {
        let Ok(read) = std::fs::read_dir(dir) else {
            return 0;
        };
        read.flatten()
            .map(|e| match e.metadata() {
                Ok(m) if m.is_dir() => walk(&e.path()),
                Ok(m) => m.len(),
                Err(_) => 0,
            })
            .sum()
    }
    if !is_safe_app_id(id) {
        return 0;
    }
    walk(&crate::app_sandbox_dir(id))
}

/// Empties an app's private storage but keeps the app installed — the
/// "Clear data" of a phone's app-info page.
pub fn clear_app_data(id: &str) {
    if !is_safe_app_id(id) {
        return;
    }
    let dir = crate::app_sandbox_dir(id);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
}

/// Removes an uninstalled app's code directory AND its private data — the OS
/// convention: uninstalling an app deletes its storage.
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

        // A Legacy version never recorded declarations, so base keeps its own.
        let legacy = crate::versions::version_of(&older, "", T, 0);
        let restored = legacy.apply_to(&base, "View{ v1 }".into());
        assert_eq!(restored.permissions, vec!["network".to_string()]);
        assert!(restored.allow_net);
        assert_eq!(restored.current_version.as_deref(), Some("20260724-153204"));
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
