//! Mini-apps as portable files: one app in, one app out.
//!
//! A generated app is just a Splash script plus a scrap of metadata, so a
//! bundle is a single JSON file — small enough to paste into a chat message,
//! self-contained enough to drop in a folder and hand to someone.
//! Format 2 includes the complete version lineage, recorded authors, and
//! historical main and widget code. Format 1 remains importable.
//!
//! Import also accepts a **bare Splash script** (`.splash`, or anything that
//! isn't JSON): the `// name:` / `// icon:` / `// tint:` header the generator
//! already writes carries everything a manifest needs, and everything else has
//! a sane default. That means an app you exported, an app you hand-wrote, and
//! an app the agent printed into a chat all import the same way.
//!
//! Neither direction touches the registry — the caller decides what to do
//! with the manifest it gets back.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{
    data_root,
    manifest::{MiniAppManifest, WidgetManifest},
    versions::{AcquisitionSource, VersionOrigin, VersionSnapshot, new_version},
};

/// Extension of an exported bundle. Deliberately not `.json`: an export is a
/// *thing you can install*, and the file listing says so.
pub const BUNDLE_EXT: &str = "splashapp";
pub const BUNDLE_MIME: &str = "application/vnd.robius.splashapp+json";
/// One bounded read may contain the working copy and every historical source.
pub const MAX_BUNDLE_BYTES: usize = 16 * 1024 * 1024;

/// Bumped only for a change old readers can't cope with. Readers accept
/// anything ≤ this; a newer bundle is rejected with a message rather than
/// silently importing half of it.
const FORMAT: u32 = 2;

/// The wire form. Flat and boring on purpose — someone will read this in a
/// text editor.
#[derive(Serialize, Deserialize)]
struct BundleFile {
    format: u32,
    /// The exporter's id. A suggestion only: the importer re-uniques it
    /// against what's already installed.
    id: String,
    name: String,
    icon: String,
    tint: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default)]
    allow_net: bool,
    /// Declared permission ids. Declarations only; the importing user's
    /// grants never travel in a bundle.
    #[serde(default)]
    permissions: Vec<String>,
    /// The app's stated reason per permission, shown at install time.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    permission_reasons: std::collections::BTreeMap<String, String>,
    /// Capability ids narrowing the declared groups (see `capabilities`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    capabilities: Vec<String>,
    #[serde(default)]
    shortcuts: Vec<String>,
    source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    widget: Option<BundleWidget>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    history: Vec<VersionSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    current_version: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct BundleWidget {
    source: String,
    default_span: (u8, u8),
    min_span: (u8, u8),
}

/// Serializes an app to bundle text. Pretty-printed: bundles get pasted into
/// chats and diffed, and the source is one long escaped string either way.
pub fn to_text(manifest: &MiniAppManifest) -> String {
    try_to_text(manifest).unwrap_or_default()
}

fn text_with_current_history(manifest: &MiniAppManifest, mut history: Vec<VersionSnapshot>) -> String {
    let mut current_version = manifest.current_version.clone().filter(|stamp|
        history.iter().any(|entry| entry.version.stamp == *stamp && entry.matches_manifest(manifest)));
    // A pristine built-in or older app may never have been edited. Include
    // its initial code without inventing a creation date or author.
    if current_version.is_none() {
        let pristine = manifest.builtin && crate::builtin::stock(&manifest.id)
            .is_some_and(|stock| crate::builtin::matches_default(manifest, &stock));
        let origin = if pristine { VersionOrigin::Stock } else { VersionOrigin::Legacy };
        let parent = manifest.current_version.as_deref().filter(|stamp|
            history.iter().any(|entry| entry.version.stamp == *stamp));
        let note = if pristine { "Original" } else if history.is_empty() {
            "Saved copy (earlier history unavailable)"
        } else { "Current saved copy (date not recorded)" };
        let mut version = new_version(manifest, origin, note, parent, 0, 0);
        if manifest.builtin {
            version.acquired_from = Some(AcquisitionSource::BuiltIn { app_id: manifest.id.clone() });
        }
        let base = version.stamp.clone();
        let mut collision = 2;
        while history.iter().any(|entry| entry.version.stamp == version.stamp) {
            version.stamp = format!("{base}-{collision}");
            collision += 1;
        }
        current_version = Some(version.stamp.clone());
        history.push(VersionSnapshot { version, source: manifest.source.clone() });
    }
    to_text_with_history(manifest, history, current_version)
}

/// Exports only a complete, readable history within the import size limit.
pub fn try_to_text(manifest: &MiniAppManifest) -> Result<String, String> {
    let history = crate::persistence::export_history(&manifest.id)
        .map_err(|error| format!("cannot export the app's complete history: {error}"))?;
    let text = text_with_current_history(manifest, history);
    parse_with_history(&text)?;
    Ok(text)
}

/// Serializes an explicit complete history, also useful for backup tooling.
pub fn to_text_with_history(
    manifest: &MiniAppManifest,
    history: Vec<VersionSnapshot>,
    current_version: Option<String>,
) -> String {
    let file = BundleFile {
        format: FORMAT,
        id: manifest.id.clone(),
        name: manifest.name.clone(),
        icon: manifest.icon.clone(),
        tint: manifest.tint,
        description: Some(manifest.description.clone()),
        allow_net: manifest.allow_net,
        permissions: manifest.permissions.clone(),
        permission_reasons: manifest.permission_reasons.clone(),
        capabilities: manifest.capabilities.clone(),
        shortcuts: manifest.shortcuts.clone(),
        source: manifest.source.clone(),
        widget: manifest.widget.as_ref().map(|w| BundleWidget {
            source: w.source.clone(),
            default_span: w.default_span,
            min_span: w.min_span,
        }),
        history,
        current_version,
    };
    // Infallible in practice (plain owned data); fall back rather than panic
    // in a UI handler.
    serde_json::to_string_pretty(&file).unwrap_or_default()
}

#[derive(Clone, Debug)]
pub struct ImportedBundle {
    pub manifest: MiniAppManifest,
    pub history: Vec<VersionSnapshot>,
    pub current_version: Option<String>,
}

/// Parses bundle text — or a bare Splash script — into a manifest.
///
/// The returned `id` is whatever the bundle suggested (or a kebab-case of the
/// name); the caller must re-unique it against the installed set. `builtin` is
/// always false: importing can't mint a protected app. `scope` is always
/// Account: room attachment is local state and never travels in a file.
pub fn parse(text: &str) -> Result<MiniAppManifest, String> {
    parse_with_history(text).map(|bundle| bundle.manifest)
}

/// Parses a portable app while retaining every restorable historical source.
pub fn parse_with_history(text: &str) -> Result<ImportedBundle, String> {
    if text.len() > MAX_BUNDLE_BYTES {
        return Err("the mini-app file is too large (maximum 16 MiB)".into());
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("nothing to import".to_string());
    }
    if trimmed.starts_with('{') {
        return parse_bundle(trimmed);
    }
    parse_bare_source(trimmed).map(|manifest| ImportedBundle {
        manifest, history: Vec::new(), current_version: None,
    })
}

fn parse_bundle(text: &str) -> Result<ImportedBundle, String> {
    let file: BundleFile = serde_json::from_str(text)
        .map_err(|e| format!("not a valid app bundle: {}", first_line(&e.to_string())))?;
    if file.format > FORMAT {
        return Err(format!(
            "bundle format {} is newer than this build understands",
            file.format
        ));
    }
    if file.source.trim().is_empty() {
        return Err("the bundle has no app source".to_string());
    }
    if file.format == 2 && file.history.is_empty() {
        return Err("the bundle is missing its version history".into());
    }
    crate::persistence::validate_history(&file.history, file.current_version.as_deref())
        .map_err(|error| format!("invalid app history: {error}"))?;
    if !file.history.is_empty() && file.current_version.is_none() {
        return Err("the bundle's history has no current version".into());
    }
    if let Some(stamp) = file.current_version.as_deref() {
        let current = file.history.iter().find(|entry| entry.version.stamp == stamp).unwrap();
        let version = &current.version;
        if current.source != file.source || version.name != file.name || version.icon != file.icon
            || version.tint != file.tint || version.allow_net != file.allow_net
            || version.description.as_ref().is_some_and(|description| Some(description) != file.description.as_ref())
            || version.permissions != file.permissions || version.permission_reasons != file.permission_reasons
            || version.capabilities != file.capabilities || version.shortcuts != file.shortcuts {
            return Err("the current history version does not match the app".into());
        }
        if version.widget_recorded {
            let historical = version.widget.as_ref().map(|widget|
                (&widget.source, widget.default_span, widget.min_span));
            let working = file.widget.as_ref().map(|widget|
                (&widget.source, widget.default_span, widget.min_span));
            if historical != working {
                return Err("the current history version does not match the app widget".into());
            }
        }
    }
    let mut manifest = MiniAppManifest {
        id: sanitize_id(&file.id, &file.name),
        name: clamp_name(&file.name),
        icon: clamp_icon(&file.icon),
        tint: file.tint,
        description: file.description.unwrap_or_else(||
            crate::header::parse_app_header(&file.source).description.unwrap_or_default()),
        source: file.source,
        allow_net: file.allow_net,
        permissions: sanitize_permissions(&file.permissions),
        // Clamp each reason: it is untrusted text rendered in host chrome.
        permission_reasons: file
            .permission_reasons
            .into_iter()
            .map(|(k, v)| (k, clamp_reason(&v)))
            .collect(),
        capabilities: file.capabilities,
        builtin: false,
        widget: file.widget.map(|w| WidgetManifest {
            source: w.source,
            default_span: w.default_span,
            min_span: w.min_span,
        }),
        shortcuts: file.shortcuts,
        scope: Default::default(),
        current_version: None,
    };
    manifest.normalize_permissions();
    let history = file.history.into_iter().map(|mut entry| {
        entry.version.imported = true;
        entry
    }).collect();
    Ok(ImportedBundle { manifest, history, current_version: file.current_version })
}

/// Keeps only permission ids this build knows, deduped. Unknown ids couldn't
/// be granted anyway; dropping them keeps the app's info honest about what
/// the app can ever do.
fn sanitize_permissions(perms: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in perms {
        if crate::permissions::Permission::from_str(p).is_some() && !out.contains(p) {
            out.push(p.clone());
        }
    }
    out
}

/// A bare `.splash` script: the header comments are the manifest.
fn parse_bare_source(source: &str) -> Result<MiniAppManifest, String> {
    let header = crate::header::parse_app_header(source);
    let name = header.name.unwrap_or_else(|| "Imported App".to_string());
    let mut manifest = MiniAppManifest {
        id: sanitize_id("", &name),
        name: clamp_name(&name),
        icon: clamp_icon(&header.icon.unwrap_or_else(|| "📦".to_string())),
        tint: header.tint.unwrap_or(0x7c6cf0),
        description: header.description.unwrap_or_default(),
        source: source.to_string(),
        allow_net: false,
        // A hand-written script declares in its header exactly like a
        // generated one; dropping that here would install an app whose own
        // code can never work.
        permissions: sanitize_permissions(&header.permissions),
        capabilities: header.capabilities,
        permission_reasons: header
            .permission_reasons
            .into_iter()
            .map(|(k, v)| (k, clamp_reason(&v)))
            .collect(),
        builtin: false,
        widget: None,
        shortcuts: Vec::new(),
        scope: Default::default(),
        current_version: None,
    };
    manifest.normalize_permissions();
    Ok(manifest)
}

/// Kebab-cases whatever the bundle claimed, falling back to the name. Never
/// trusts the file: an id becomes a directory name under `apps/`, so anything
/// that isn't `[a-z0-9-]` is dropped here rather than caught downstream.
pub fn sanitize_id(id: &str, name: &str) -> String {
    let from = if id.trim().is_empty() { name } else { id };
    let kebab = from
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if kebab.is_empty() {
        "imported".to_string()
    } else {
        kebab.chars().take(40).collect()
    }
}

/// Trims an app-supplied permission reason to something that fits a prompt
/// line. It is the app's own words rendered in host chrome, so it is clamped
/// hard and stripped of newlines — a reason cannot become a wall of text or
/// fake extra dialog copy.
fn clamp_reason(reason: &str) -> String {
    reason
        .replace(['\n', '\r'], " ")
        .trim()
        .chars()
        .take(120)
        .collect()
}

fn clamp_name(name: &str) -> String {
    let n: String = name.trim().chars().take(18).collect();
    if n.is_empty() { "Imported App".to_string() } else { n }
}

fn clamp_icon(icon: &str) -> String {
    let i: String = icon.trim().chars().take(12).collect();
    if i.is_empty() { "📦".to_string() } else { i }
}

fn first_line(msg: &str) -> String {
    msg.lines().next().unwrap_or(msg).chars().take(90).collect()
}

// ---------------------------------------------------------------------------
// The exchange folder
// ---------------------------------------------------------------------------

/// Where exports are written and imports are looked for. One folder for both
/// directions: exporting an app makes it visible to Import on the next device
/// that syncs the folder, and dropping a file in makes it importable here
/// without any picker.
pub fn exchange_dir() -> PathBuf {
    data_root().join("exchange")
}

/// Writes `manifest` into the exchange folder, returning the path. Overwrites
/// a previous export of the same app — the folder is a drop box, not history.
pub fn write_export(manifest: &MiniAppManifest) -> Result<PathBuf, String> {
    let dir = exchange_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let path = dir.join(format!("{}.{BUNDLE_EXT}", sanitize_id(&manifest.id, &manifest.name)));
    std::fs::write(&path, try_to_text(manifest)?.as_bytes())
        .map_err(|e| format!("can't write {}: {e}", path.display()))?;
    Ok(path)
}

/// The caption that goes with a bundle sent into a room.
pub fn share_caption(manifest: &MiniAppManifest) -> String {
    let mut caption = format!(
        "{} {} · a Robrix mini-app bundle. Save the file and import it from the Mini Apps screen to run it.",
        manifest.icon, manifest.name,
    );
    if !manifest.permissions.is_empty() {
        caption.push_str(&format!(" It asks for: {}.", manifest.permissions.join(", ")));
    }
    caption
}

/// One importable file found in the exchange folder.
#[derive(Clone, Debug)]
pub struct ImportEntry {
    pub path: PathBuf,
    /// The app's display name, read from the file (not the file name).
    pub name: String,
    pub icon: String,
    /// The file it came from, for the row's second line.
    pub detail: String,
    /// Permission ids the file's manifest declares. Carried through so the
    /// row can say what the app may ever ask for BEFORE it's installed —
    /// declarations are the only honest thing to show at that point.
    pub permissions: Vec<String>,
}

/// Everything importable in the exchange folder, by name. Files that don't
/// parse are left out rather than listed as broken rows: the folder is
/// user-writable and may hold anything.
pub fn list_importable() -> Vec<ImportEntry> {
    let Ok(entries) = std::fs::read_dir(exchange_dir()) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !matches!(ext, BUNDLE_EXT | "splash" | "json") {
            continue;
        }
        // Cap the read: this folder is user-writable, and a stray gigabyte
        // file must not stall the picker.
        let Ok(meta) = entry.metadata() else { continue };
        if meta.len() > MAX_BUNDLE_BYTES as u64 {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(manifest) = parse(&text) else { continue };
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        found.push(ImportEntry {
            name: manifest.name,
            icon: manifest.icon,
            detail: file_name.clone(),
            permissions: manifest.permissions,
            path,
        });
    }
    found.sort_by_key(|e| e.name.to_lowercase());
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> MiniAppManifest {
        MiniAppManifest {
            id: "pomodoro".to_string(),
            name: "Pomodoro".to_string(),
            icon: "🍅".to_string(),
            tint: 0xE84D3D,
            description: String::new(),
            source: "// name: Pomodoro\nView{}".to_string(),
            allow_net: false,
            permissions: vec!["network".to_string(), "open-url".to_string()],
            permission_reasons: Default::default(),
            capabilities: Vec::new(),
            builtin: true,
            widget: None,
            shortcuts: vec!["Start".to_string()],
            scope: Default::default(),
            current_version: Some("20260724-153204".to_string()),
        }
    }

    #[test]
    fn round_trips_a_bundle() {
        let mut initial = sample();
        initial.source = "// name: Pomodoro\n// description: A header description\nView{}".into();
        // An explicitly empty saved description remains empty, rather than
        // being replaced by the source header on import.
        let text = to_text(&initial);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&text).unwrap()["format"], 2);
        // The pointer travels with its matching historical source, separately
        // from the installed manifest until the caller imports the history.
        let bundle = parse_with_history(&text).unwrap();
        assert_eq!(bundle.history.len(), 1);
        assert_eq!(bundle.history[0].version.at_unix, 0);
        assert!(bundle.history[0].version.actor.is_none());
        assert!(bundle.history[0].version.imported);
        assert_eq!(bundle.current_version.as_deref(), Some(bundle.history[0].version.stamp.as_str()));
        let m = parse(&text).unwrap();
        assert!(m.current_version.is_none());
        assert_eq!(m.id, "pomodoro");
        assert_eq!(m.name, "Pomodoro");
        assert_eq!(m.icon, "🍅");
        assert_eq!(m.tint, 0xE84D3D);
        assert!(m.description.is_empty());
        assert_eq!(m.shortcuts, vec!["Start".to_string()]);
        // Declarations travel; grants never do (they're host state).
        assert_eq!(m.permissions, vec!["network".to_string(), "open-url".to_string()]);
        // Legacy readers keep working: network declared -> allow_net mirrored.
        assert!(m.allow_net);
        // An import can never mint a protected app, whatever the file says.
        assert!(!m.builtin);
    }

    #[test]
    fn old_bundles_have_no_invented_history_or_author() {
        let bundle = parse_with_history(r#"{"format":1,"id":"x","name":"X","icon":"x","tint":0,"source":"View{}"}"#).unwrap();
        assert!(bundle.history.is_empty());
        assert!(bundle.current_version.is_none());
    }

    #[test]
    fn shares_all_authors_historical_code_and_parent_links() {
        use crate::versions::VersionActor;
        let mut manifest = sample();
        manifest.id = "portable-history-source".into();
        manifest.builtin = false;
        manifest.widget = Some(WidgetManifest {
            source: "View{ widget_v1 }".into(), default_span: (2, 2), min_span: (1, 1),
        });
        manifest.normalize_permissions();
        let mut first = new_version(&manifest, VersionOrigin::Ai, "Create a timer", None, 1_784_907_124, 0);
        first.actor = Some(VersionActor { user_id: "@alice:example.org".into(), display_name: Some("Alice".into()) });
        first.host_version = Some("0.4.0".into());
        first.host_revision = Some("alice-revision".into());
        let first_stamp = crate::persistence::append_version(&manifest, first).unwrap();
        manifest.source = "// name: Pomodoro\nView{ updated }".into();
        manifest.widget.as_mut().unwrap().source = "View{ widget_v2 }".into();
        let mut second = new_version(&manifest, VersionOrigin::Manual, "Add pause", Some(&first_stamp), 1_784_907_125, 0);
        second.actor = Some(VersionActor { user_id: "@bob:example.org".into(), display_name: None });
        manifest.current_version = Some(crate::persistence::append_version(&manifest, second).unwrap());
        let mut bundle = parse_with_history(&try_to_text(&manifest).unwrap()).unwrap();
        assert_eq!(bundle.history.len(), 2);
        assert_eq!(bundle.history[0].version.actor.as_ref().unwrap().user_id, "@alice:example.org");
        assert_eq!(bundle.history[0].version.host_version.as_deref(), Some("0.4.0"));
        assert_eq!(bundle.history[0].version.host_revision.as_deref(), Some("alice-revision"));
        assert_eq!(bundle.history[1].version.actor.as_ref().unwrap().user_id, "@bob:example.org");
        assert_eq!(bundle.history[1].version.parent.as_deref(), Some(first_stamp.as_str()));
        assert!(bundle.history.iter().all(|snapshot| snapshot.version.imported));
        assert_eq!(bundle.history[0].source, sample().source);
        bundle.manifest.id = "portable-history-destination".into();
        crate::persistence::import_history(&mut bundle.manifest, &bundle.history, bundle.current_version.as_deref()).unwrap();
        let imported = crate::persistence::load_version(&bundle.manifest.id, &first_stamp).unwrap();
        assert_eq!(imported.1, sample().source);
        let restored = imported.0.apply_to(&bundle.manifest, imported.1);
        assert_eq!(restored.source, sample().source);
        assert_eq!(restored.widget.as_ref().unwrap().source, "View{ widget_v1 }");
        assert!(!restored.builtin);
        let mut acquisition = new_version(&bundle.manifest, VersionOrigin::Import, "Imported this file",
            bundle.manifest.current_version.as_deref(), 1_784_907_126, 0);
        acquisition.actor = Some(VersionActor { user_id: "@charlie:example.org".into(), display_name: None });
        acquisition.acquired_from = Some(AcquisitionSource::RoomAttachment {
            room_id: "!room:example.org".into(), event_id: Some("$message".into()),
            media_uri: Some("mxc://example.org/file".into()), shared_at_unix: Some(1_784_907_125),
            file_name: "pomodoro.splashapp".into(),
            sender: Some(VersionActor { user_id: "@bob:example.org".into(), display_name: Some("Bob".into()) }),
        });
        bundle.manifest.current_version = Some(crate::persistence::append_version(&bundle.manifest, acquisition).unwrap());
        let relayed = parse_with_history(&try_to_text(&bundle.manifest).unwrap()).unwrap();
        assert_eq!(relayed.history.len(), 3);
        assert_eq!(relayed.history[0].version.actor.as_ref().unwrap().user_id, "@alice:example.org");
        assert_eq!(relayed.history[2].version.actor.as_ref().unwrap().user_id, "@charlie:example.org");
        assert!(matches!(&relayed.history[2].version.acquired_from,
            Some(AcquisitionSource::RoomAttachment { shared_at_unix: Some(1_784_907_125), sender: Some(sender), .. })
                if sender.user_id == "@bob:example.org"));
        crate::persistence::remove_user_app(&manifest.id);
        crate::persistence::remove_user_app(&bundle.manifest.id);
    }

    #[test]
    fn refuses_history_that_disagrees_with_the_working_copy() {
        let manifest = sample();
        let version = new_version(&manifest, VersionOrigin::Manual, "", None, 1_784_907_124, 0);
        let stamp = version.stamp.clone();
        let text = to_text_with_history(&manifest, vec![VersionSnapshot { version, source: "View{ other }".into() }], Some(stamp));
        assert!(parse_with_history(&text).unwrap_err().contains("does not match"));
        let mut version = new_version(&manifest, VersionOrigin::Manual, "", None, 1_784_907_124, 0);
        version.permissions.clear();
        let stamp = version.stamp.clone();
        let text = to_text_with_history(&manifest, vec![VersionSnapshot { version, source: manifest.source.clone() }], Some(stamp));
        assert!(parse_with_history(&text).unwrap_err().contains("does not match"));
        let version = new_version(&manifest, VersionOrigin::Manual, "", None, 1_784_907_124, 0);
        let text = to_text_with_history(&manifest, vec![VersionSnapshot { version, source: manifest.source.clone() }], None);
        assert!(parse_with_history(&text).unwrap_err().contains("no current version"));
        assert!(parse_with_history(r#"{"format":2,"id":"x","name":"X","icon":"x","tint":0,"source":"View{}"}"#)
            .unwrap_err().contains("missing its version history"));
    }

    #[test]
    fn a_modified_builtin_with_missing_history_never_claims_to_be_stock() {
        let mut manifest = crate::builtin::stock("room-info").unwrap();
        let pristine = parse_with_history(&try_to_text(&manifest).unwrap()).unwrap();
        assert_eq!(pristine.history.last().unwrap().version.origin, VersionOrigin::Stock);
        manifest.source.push_str("\n// locally modified");
        let modified = parse_with_history(&try_to_text(&manifest).unwrap()).unwrap();
        let version = &modified.history.last().unwrap().version;
        assert_eq!(version.origin, VersionOrigin::Legacy);
        assert_eq!(version.at_unix, 0);
        assert!(version.actor.is_none());
        assert!(matches!(&version.acquired_from,
            Some(AcquisitionSource::BuiltIn { app_id }) if app_id == "room-info"));
    }

    #[test]
    fn damaged_history_is_reported_on_export() {
        let mut manifest = sample();
        manifest.id = "portable-history-damaged".into();
        let version = new_version(&manifest, VersionOrigin::Manual, "", Some("missing-parent"), 1_784_907_124, 0);
        manifest.current_version = Some(crate::persistence::append_version(&manifest, version).unwrap());
        assert!(try_to_text(&manifest).unwrap_err().contains("missing parent"));
        crate::persistence::remove_user_app(&manifest.id);
    }

    #[test]
    fn unknown_and_duplicate_permissions_are_dropped_on_import() {
        let text = r#"{"format":1,"id":"x","name":"X","icon":"x","tint":0,"source":"View{}",
            "permissions":["network","made-up-cap","network","location"]}"#;
        let m = parse(text).unwrap();
        assert_eq!(m.permissions, vec!["network".to_string(), "location".to_string()]);
    }

    #[test]
    fn imports_a_bare_splash_script() {
        let m = parse("// name: Tip Jar\n// icon: 💰\n// tint: #112233\nView{}").unwrap();
        assert_eq!(m.name, "Tip Jar");
        assert_eq!(m.id, "tip-jar");
        assert_eq!(m.icon, "💰");
        assert_eq!(m.tint, 0x112233);
    }

    #[test]
    fn rejects_junk_and_future_formats() {
        assert!(parse("   ").is_err());
        assert!(parse("{ not json").is_err());
        assert!(parse(r#"{"format":99,"id":"x","name":"X","icon":"x","tint":0,"source":"View{}"}"#)
            .is_err());
        assert!(parse(r#"{"format":1,"id":"x","name":"X","icon":"x","tint":0,"source":"  "}"#)
            .is_err());
    }

    #[test]
    fn ids_from_a_bundle_can_never_escape_the_apps_dir() {
        let text = r#"{"format":1,"id":"../../etc","name":"X","icon":"x","tint":0,"source":"View{}"}"#;
        assert_eq!(parse(text).unwrap().id, "etc");
    }
}
