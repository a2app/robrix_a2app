//! The manifests of the pre-installed mini-apps.
//!
//! Splash sources live in the `apps/` directory. They're baked into the binary,
//! but in a dev checkout we prefer reading them from disk so `.splash` edits
//! show up on the next app launch without a rebuild.

use crate::manifest::MiniAppManifest;
use crate::versions::{VersionOrigin, VersionSnapshot};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The latest shipped default recorded for an installed built-in app.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuiltinBaseline {
    pub fingerprint: String,
    pub stamp: String,
}

#[derive(Clone, Debug)]
pub struct BuiltinReconciliation {
    pub manifest: MiniAppManifest,
    pub baseline: BuiltinBaseline,
    pub available_update: Option<String>,
    pub adopted_default: bool,
}

/// Fingerprints the complete portable default, excluding installation state.
pub fn builtin_fingerprint(manifest: &MiniAppManifest) -> Result<String, String> {
    let mut permissions = manifest.permissions.iter().collect::<Vec<_>>();
    permissions.sort();
    permissions.dedup();
    let mut capabilities = manifest.capabilities.iter().collect::<Vec<_>>();
    capabilities.sort();
    capabilities.dedup();
    let bytes = serde_json::to_vec(&(
        &manifest.name, &manifest.icon, manifest.tint, &manifest.description,
        &manifest.source, &manifest.widget, manifest.allow_net, permissions,
        &manifest.permission_reasons, capabilities, &manifest.shortcuts,
    )).map_err(|error| format!("Could not fingerprint the built-in default: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

pub fn matches_default(current: &MiniAppManifest, stock: &MiniAppManifest) -> bool {
    match (builtin_fingerprint(current), builtin_fingerprint(stock)) {
        (Ok(current), Ok(stock)) => current == stock,
        _ => false,
    }
}

fn snapshot_manifest(base: &MiniAppManifest, snapshot: &VersionSnapshot) -> MiniAppManifest {
    let version = &snapshot.version;
    MiniAppManifest {
        name: version.name.clone(), icon: version.icon.clone(), tint: version.tint,
        description: version.description.clone().unwrap_or_else(||
            crate::header::parse_app_header(&snapshot.source).description.unwrap_or_default()),
        source: snapshot.source.clone(), allow_net: version.allow_net,
        permissions: version.permissions.clone(), permission_reasons: version.permission_reasons.clone(),
        capabilities: version.capabilities.clone(), shortcuts: version.shortcuts.clone(),
        widget: version.widget.clone(), current_version: Some(version.stamp.clone()),
        ..base.clone()
    }
}

fn local_stock_snapshot(id: &str, stamp: &str) -> Option<VersionSnapshot> {
    crate::persistence::load_version(id, stamp).and_then(|(version, source)|
        (version.origin == VersionOrigin::Stock && !version.imported)
            .then_some(VersionSnapshot { version, source }))
}

fn migrate_default_declarations(manifest: &mut MiniAppManifest, stock: &MiniAppManifest) {
    for permission in &stock.permissions {
        if !manifest.permissions.contains(permission) { manifest.permissions.push(permission.clone()); }
    }
    for (permission, reason) in &stock.permission_reasons {
        manifest.permission_reasons.entry(permission.clone()).or_insert_with(|| reason.clone());
    }
    manifest.normalize_permissions();
}

// A comment-only edit still creates a customized version, so it correctly
// does not adopt later defaults wholesale. Recognize the old, locally archived
// Search template before repairing its known fixed-height layout in place.
// Executable edits and imported Stock claims never qualify for this repair.
fn source_without_comment_lines(source: &str) -> String {
    source.lines().filter(|line| {
        let line = line.trim_start();
        !line.is_empty() && !line.starts_with("//")
    }).collect::<Vec<_>>().join("\n")
}

fn legacy_search_layout(source: &str) -> Option<String> {
    let fragments = [
        ("View{\n    width: Fill height: Fit flow: Down align: Align{x: 0.5}",
         "View{\n    width: Fill height: Fill flow: Down align: Align{x: 0.5}"),
        ("col := View{\n        width: Fill{max: 520.0} height: Fit flow: Down spacing: 10 padding: 14",
         "col := ScrollYView{\n        width: Fill height: Fill flow: Down spacing: 10 padding: 14"),
        ("result_list := ScrollYView{\n            width: Fill height: 300 flow: Down spacing: 5",
         "result_list := ScrollYView{\n            width: Fill height: Fill{min: 80.0} flow: Down spacing: 5"),
    ];
    // The complete old layout must match. Do not rewrite arbitrary custom
    // widgets merely because they happen to have a 300-pixel dimension.
    if !fragments.iter().all(|(old, _)| source.matches(old).count() == 1) {
        return None;
    }
    let mut updated = source.to_string();
    for (old, new) in fragments { updated = updated.replacen(old, new, 1); }
    updated = updated.replacen(
        "room_list := ScrollYView{\n                width: Fill height: 160 flow: Down spacing: 3",
        "room_list := ScrollYView{\n                width: Fill height: Fit{max: FitBound.Rel{base: Base.Full, factor: 0.3}} flow: Down spacing: 3",
        1,
    );
    Some(updated)
}

fn repair_legacy_search(
    manifest: &mut MiniAppManifest,
    stock: &MiniAppManifest,
    at_unix: u64,
    offset_secs: i64,
    host_version: Option<&str>,
    host_revision: Option<&str>,
) -> anyhow::Result<()> {
    if manifest.id != "search" { return Ok(()); }
    let Some(source) = legacy_search_layout(&manifest.source) else { return Ok(()) };
    let mut working = manifest.clone();
    working.source = source_without_comment_lines(&working.source);
    migrate_default_declarations(&mut working, stock);
    let known_template = crate::persistence::list_versions(&manifest.id).into_iter()
        .filter(|version| version.origin == VersionOrigin::Stock && !version.imported)
        .filter_map(|version| local_stock_snapshot(&manifest.id, &version.stamp))
        .any(|snapshot| {
            let mut previous = snapshot_manifest(stock, &snapshot);
            previous.source = source_without_comment_lines(&previous.source);
            migrate_default_declarations(&mut previous, stock);
            matches_default(&working, &previous)
        });
    if !known_template { return Ok(()); }
    let parent = manifest.current_version.clone();
    manifest.source = source;
    let mut version = crate::versions::new_version(manifest, VersionOrigin::Legacy,
        "Robrix repaired the built-in Search layout; local edits preserved", parent.as_deref(), at_unix, offset_secs);
    version.host_version = host_version.filter(|version| !version.is_empty()).map(str::to_string);
    version.host_revision = host_revision.filter(|revision| !revision.is_empty()).map(str::to_string);
    manifest.current_version = Some(crate::persistence::append_version(manifest, version)?);
    Ok(())
}

/// Reconciles a shipped default while preserving the user's customized branch.
///
/// New defaults form their own Stock lineage. Untouched apps adopt them;
/// customized apps retain their source and receive an optional update stamp.
pub fn reconcile_builtin(
    current: &MiniAppManifest,
    stock: &MiniAppManifest,
    baseline: Option<&BuiltinBaseline>,
    pending_stamp: Option<&str>,
    at_unix: u64,
    offset_secs: i64,
) -> anyhow::Result<BuiltinReconciliation> {
    reconcile_builtin_with_host(current, stock, baseline, pending_stamp, at_unix, offset_secs, None, None)
}

/// Records which Robrix release supplied a newly archived built-in default.
pub fn reconcile_builtin_with_host(
    current: &MiniAppManifest,
    stock: &MiniAppManifest,
    baseline: Option<&BuiltinBaseline>,
    pending_stamp: Option<&str>,
    at_unix: u64,
    offset_secs: i64,
    host_version: Option<&str>,
    host_revision: Option<&str>,
) -> anyhow::Result<BuiltinReconciliation> {
    if current.id != stock.id || !current.builtin || !stock.builtin {
        anyhow::bail!("built-in reconciliation requires the same installed built-in app");
    }
    let fingerprint = builtin_fingerprint(stock).map_err(anyhow::Error::msg)?;
    let mut previous = baseline.and_then(|baseline| {
        let snapshot = local_stock_snapshot(&current.id, &baseline.stamp)?;
        (builtin_fingerprint(&snapshot_manifest(stock, &snapshot)).ok().as_deref()
            == Some(baseline.fingerprint.as_str())).then_some(snapshot)
    });
    let authenticated_baseline = previous.is_some();
    // A local Stock pointer predates release tracking but still records the
    // user's unmodified default. Imported claims never establish that fact.
    if previous.is_none() {
        previous = current.current_version.as_deref().and_then(|stamp|
            local_stock_snapshot(&current.id, stamp));
    }
    let previous_fingerprint = previous.as_ref().and_then(|snapshot|
        builtin_fingerprint(&snapshot_manifest(stock, snapshot)).ok());
    let changed = previous_fingerprint.as_deref().is_some_and(|previous| previous != fingerprint);
    let follows_previous = previous.as_ref().is_some_and(|snapshot| {
        let mut previous = snapshot_manifest(stock, snapshot);
        let mut working = current.clone();
        migrate_default_declarations(&mut previous, stock);
        migrate_default_declarations(&mut working, stock);
        matches_default(&working, &previous)
    });
    let matches_current_stock = matches_default(current, stock);
    let mut manifest = current.clone();
    if !matches_current_stock && !follows_previous {
        crate::persistence::ensure_current_version(&mut manifest, VersionOrigin::Legacy,
            "Saved customized copy (date not recorded)", 0, 0)?;
        repair_legacy_search(&mut manifest, stock, at_unix, offset_secs, host_version, host_revision)?;
    }
    // Reuse an already archived default when a state-file save was interrupted.
    let existing = if previous_fingerprint.as_deref() == Some(fingerprint.as_str()) {
        previous.clone()
    } else {
        let parent = previous.as_ref().map(|snapshot| snapshot.version.stamp.as_str());
        crate::persistence::list_versions(&stock.id).into_iter().rev()
            .filter(|version| version.origin == VersionOrigin::Stock && !version.imported && version.actor.is_none()
                && version.parent.as_deref() == parent)
            .filter_map(|version| local_stock_snapshot(&stock.id, &version.stamp))
            .find(|snapshot| builtin_fingerprint(&snapshot_manifest(stock, snapshot)).ok().as_deref()
                == Some(fingerprint.as_str()))
    };
    let stamp = if let Some(snapshot) = existing {
        snapshot.version.stamp
    } else {
        let parent = previous.as_ref().map(|snapshot| snapshot.version.stamp.as_str());
        let time = if changed { at_unix } else { 0 };
        let note = if changed { "Updated built-in default" } else { "Built-in default" };
        let mut version = crate::versions::new_version(stock, VersionOrigin::Stock, note, parent, time, offset_secs);
        version.host_version = host_version.filter(|version| !version.is_empty()).map(str::to_string);
        version.host_revision = host_revision.filter(|revision| !revision.is_empty()).map(str::to_string);
        crate::persistence::append_version(stock, version)?
    };
    let available_update = if matches_current_stock || follows_previous {
        manifest = stock.clone();
        manifest.scope = current.scope.clone();
        manifest.current_version = Some(stamp.clone());
        None
    } else if changed || !authenticated_baseline || pending_stamp.is_some() {
        Some(stamp.clone())
    } else { None };
    Ok(BuiltinReconciliation {
        manifest,
        baseline: BuiltinBaseline { fingerprint, stamp },
        available_update,
        adopted_default: changed && (matches_current_stock || follows_previous),
    })
}

fn load_source(file: &str, baked: &'static str) -> String {
    let dev_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("apps")
        .join(file);
    std::fs::read_to_string(dev_path).unwrap_or_else(|_| baked.to_string())
}

macro_rules! app_source {
    ($file:literal) => {
        load_source($file, include_str!(concat!("../apps/", $file)))
    };
}

fn app(id: &str, name: &str, icon: &str, tint: u32, source: String) -> MiniAppManifest {
    let header = crate::header::parse_app_header(&source);
    let description = header.description.unwrap_or_default();
    let mut manifest = MiniAppManifest {
        id: id.to_string(),
        name: name.to_string(),
        icon: icon.to_string(),
        tint,
        description,
        source,
        allow_net: false,
        permissions: permissions_for(id),
        permission_reasons: reasons_for(id),
        capabilities: header.capabilities,
        builtin: true,
        widget: None,
        shortcuts: Vec::new(),
        scope: Default::default(),
        current_version: None,
    };
    manifest.normalize_permissions();
    manifest
}

/// Unions a built-in's stock declarations back into a saved or restored copy,
/// since a copy made before a declaration existed would otherwise lose it.
pub fn union_stock_declarations(manifest: &mut MiniAppManifest) {
    if !manifest.builtin {
        return;
    }
    for p in permissions_for(&manifest.id) {
        if !manifest.permissions.contains(&p) {
            manifest.permissions.push(p);
        }
    }
    for (perm, why) in reasons_for(&manifest.id) {
        manifest.permission_reasons.entry(perm).or_insert(why);
    }
}

/// What each stock app DECLARES. Declaring is not granting: runtime-tier
/// entries still prompt on first use. Apps absent here are fully sandboxed
/// on purpose — don't add "just in case" entries.
fn permissions_for(id: &str) -> Vec<String> {
    let p: &[&str] = match id {
        "public-web" => &["network"],
        "website-watch" => &["network", "notifications", "matrix-room-send"],
        "reminder" => &["notifications"],
        "keyword-alert" => &["matrix-room-watch", "notifications"],
        "room-peek" => &["matrix-room-info", "matrix-room-read", "matrix-room-send", "robrix-navigation", "matrix-room-watch"],
        "roll-call" => &["matrix-room-send"],
        "room-info" => &["matrix-room-info"],
        "room-members" | "room-threads" => &["matrix-room-read", "robrix-navigation", "matrix-room-watch"],
        "search" => &["matrix-room-read", "matrix-rooms-list", "matrix-rooms-read", "robrix-navigation"],
        "simple-watcher" => &["matrix-room-watch", "notifications", "matrix-room-send"],
        "watcher" => &["matrix-room-watch", "notifications", "matrix-room-send", "mcp-tools"],
        "presence" => &["matrix-room-read", "matrix-room-watch", "robrix-navigation"],
        "room-tools" => &["matrix-room-read", "matrix-room-info", "matrix-room-manage", "clipboard-write", "robrix-navigation"],
        "spaces" => &["matrix-spaces", "matrix-rooms-list", "robrix-navigation", "matrix-membership"],
        "inbox" => &["matrix-rooms-list", "robrix-navigation", "matrix-membership"],
        "room-stats" => &["matrix-room-read", "matrix-room-info", "robrix-navigation"],
        "account" => &["matrix-profile", "matrix-account-read", "matrix-users", "open-url", "robrix-navigation"],
        "inspector" => &["robrix-ui", "robrix-preferences", "robrix-observe", "device-info"],
        "room-pins" => &["matrix-room-read", "robrix-navigation", "matrix-room-info"],
        _ => &[],
    };
    p.iter().map(|x| x.to_string()).collect()
}

/// Why each stock app wants what it declares, in the APP's voice — iOS's
/// usage strings. Kept in step with the `// why-` lines in each app's own
/// `.splash` header.
fn reasons_for(id: &str) -> std::collections::BTreeMap<String, String> {
    let r: &[(&str, &str)] = match id {
        "public-web" => &[("network", "Fetches https://example.com/ when you press the button.")],
        "website-watch" => &[
            ("network", "Fetches the URL you save when your background task runs."),
            ("notifications", "Shows a popup when the keyword first appears."),
            ("matrix-room-send", "Optionally reports a match to this task's attached room."),
        ],
        "reminder" => &[("notifications", "Shows the reminder text you save when your task is due.")],
        "keyword-alert" => &[
            ("matrix-room-watch", "Tests new messages in this task's attached room for your keyword."),
            ("notifications", "Shows one popup for each matching batch of messages."),
        ],
        "room-peek" => &[
            ("matrix-room-info", "Shows this room's name and member count."),
            ("matrix-room-read", "Lists the latest messages in this room and their reactions."),
            ("matrix-room-send", "Sends the message you type into this room."),
            ("robrix-navigation", "Jumps to a message you tap."),
            ("matrix-room-watch", "Shows new messages, typing, edits and reactions as they arrive."),
        ],
        "roll-call" => &[
            ("matrix-room-send", "Posts your roll into this room."),
        ],
        "room-info" => &[
            ("matrix-room-info", "Shows this room's name, topic, and settings."),
        ],
        "room-members" => &[
            ("matrix-room-read", "Lists who is in this room."),
            ("robrix-navigation", "Opens the profile of a member you tap."),
            ("matrix-room-watch", "Refreshes the list as people join or leave."),
        ],
        "room-pins" => &[
            ("matrix-room-read", "Shows this room's pinned messages."),
            ("robrix-navigation", "Jumps to a pinned message you tap."),
            ("matrix-room-info", "Refreshes when the pins change."),
        ],
        "room-threads" => &[
            ("matrix-room-read", "Lists the discussion threads in this room and shows the one you open."),
            ("robrix-navigation", "Opens the thread you're reading in Robrix."),
            ("matrix-room-watch", "Refreshes as new messages arrive."),
        ],
        "search" => &[
            ("matrix-room-read", "Searches the messages in this room."),
            ("matrix-rooms-list", "Lists your rooms so you can pick which to search."),
            ("matrix-rooms-read", "Searches messages across the rooms you pick."),
            ("robrix-navigation", "Jumps to a result you tap."),
        ],
        "simple-watcher" => &[
            ("matrix-room-watch", "Sees new messages so it can match your rules."),
            ("notifications", "Tells you when a message matches a rule."),
            ("matrix-room-send", "Posts your reply when a rule says to."),
        ],
        "watcher" => &[
            ("matrix-room-watch", "Sees new messages so it can match your rules."),
            ("notifications", "Tells you when a message matches a rule."),
            ("matrix-room-send", "Posts your reply when a rule says to."),
            ("mcp-tools", "Lets this room's AI add rules, once you turn that on."),
        ],
        "presence" => &[
            ("matrix-room-read", "Shows how far each person has read."),
            ("matrix-room-watch", "Updates who is typing and reading as it happens."),
            ("robrix-navigation", "Opens a person you tap, or jumps to the message they read."),
        ],
        "room-tools" => &[
            ("matrix-room-read", "Lists recent messages and which of them are pinned."),
            ("matrix-room-info", "Reads the unread flag, room links and where an upgraded room went."),
            ("matrix-room-manage", "Changes favorite, low priority, unread and pins when you tap."),
            ("clipboard-write", "Copies a room or message link when you tap."),
            ("robrix-navigation", "Opens the upgraded room when you tap."),
        ],
        "spaces" => &[
            ("matrix-spaces", "Lists your spaces and the rooms inside them."),
            ("matrix-rooms-list", "Previews a room you have not joined yet."),
            ("robrix-navigation", "Opens a room you tap."),
            ("matrix-membership", "Joins a room when you tap Join."),
        ],
        "inbox" => &[
            ("matrix-rooms-list", "Lists your invites and the rooms with unread messages."),
            ("robrix-navigation", "Opens a room you tap."),
            ("matrix-membership", "Accepts or declines an invite when you tap a button."),
        ],
        "room-stats" => &[
            ("matrix-room-read", "Reads this room's messages to count who posts and when."),
            ("matrix-room-info", "Shows your power level and what it lets you do here."),
            ("robrix-navigation", "Opens the profile of a sender you tap."),
        ],
        "account" => &[
            ("matrix-profile", "Shows your display name and user id."),
            ("matrix-account-read", "Shows this device, your homeserver, and who you ignore."),
            ("matrix-users", "Looks up the profile and DM of a user id you enter."),
            ("open-url", "Opens your homeserver's account management page."),
            ("robrix-navigation", "Opens the DM you already have with a user you look up."),
        ],
        "inspector" => &[
            ("robrix-ui", "Moves, pops out, minimizes, restores or closes its own pane when you tap."),
            ("robrix-preferences", "Shows Robrix's display settings and follows changes to them."),
            ("robrix-observe", "Logs which room and screen you switch to, so you can see what an app would see."),
            ("device-info", "Shows what this device reports about itself."),
        ],
        _ => &[],
    };
    r.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// The stock manifest of one built-in.
pub fn stock(id: &str) -> Option<MiniAppManifest> {
    builtin_apps().into_iter().find(|m| m.id == id)
}

/// The pre-installed apps: always present, can't be uninstalled.
pub fn builtin_apps() -> Vec<MiniAppManifest> {
    vec![
        app("public-web", "Public Web", "🌐", 0x2A7F92, app_source!("public_web.splash")),
        app("website-watch", "Website Watch", "🌐", 0x2A7F92, app_source!("website_watch.splash")),
        app("reminder", "Reminder", "⏰", 0x8A5FB0, app_source!("reminder.splash")),
        app("keyword-alert", "Keyword Alert", "🔔", 0xD9822B, app_source!("keyword_alert.splash")),
        app("room-peek", "Room Peek", "👀", 0x4A90D9, app_source!("room_peek.splash")),
        app("roll-call", "Roll Call", "🎲", 0x7C6CF0, app_source!("roll_call.splash")),
        app("room-info", "Room Info", "🏷", 0x2E86AB, app_source!("room_info.splash")),
        app("room-members", "Room Members", "👥", 0x6C8E3A, app_source!("room_members.splash")),
        app("room-pins", "Pinned Messages", "📌", 0xC0533E, app_source!("room_pins.splash")),
        app("room-threads", "Room Threads", "🧵", 0x8A5CA8, app_source!("room_threads.splash")),
        app("search", "Search", "🔍", 0x0F88FE, app_source!("search.splash")),
        app("simple-watcher", "Simple Watcher", "👁", 0xE09F3E, app_source!("simple_watcher.splash")),
        app("watcher", "Watcher", "👁", 0xD9822B, app_source!("watcher.splash")),
        app("presence", "Who's Here", "👀", 0x3A86FF, app_source!("presence.splash")),
        app("room-tools", "Room Tools", "🧰", 0x8D6E63, app_source!("room_tools.splash")),
        app("spaces", "Spaces", "🪐", 0x5B3FD9, app_source!("spaces.splash")),
        app("inbox", "Inbox", "📥", 0xE0862B, app_source!("inbox.splash")),
        app("room-stats", "Room Stats", "📊", 0x2A9D8F, app_source!("room_stats.splash")),
        app("account", "Account", "🪪", 0x386FA4, app_source!("account.splash")),
        app("inspector", "Inspector", "🛠", 0x546E7A, app_source!("inspector.splash")),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const UPDATE_TIME: u64 = 1_784_907_124;

    fn release_app(id: &str) -> MiniAppManifest {
        let mut manifest = stock("room-info").unwrap();
        manifest.id = id.into();
        manifest.source = "View{ original_default }".into();
        manifest.description = "Original description".into();
        manifest
    }

    #[test]
    fn first_start_seeds_defaults_without_reporting_an_update() {
        let current = release_app("release-first-start");
        let reconciled = reconcile_builtin(&current, &current, None, None, UPDATE_TIME, 0).unwrap();
        assert!(reconciled.available_update.is_none());
        assert!(!reconciled.adopted_default);
        let history = crate::persistence::list_versions(&current.id);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].at_unix, 0);
        assert_eq!(history[0].origin, VersionOrigin::Stock);
        let repeated = reconcile_builtin(&reconciled.manifest, &current, Some(&reconciled.baseline), None, UPDATE_TIME + 1, 0).unwrap();
        assert_eq!(repeated.baseline, reconciled.baseline);
        assert_eq!(crate::persistence::list_versions(&current.id).len(), 1);
        crate::persistence::remove_user_app(&current.id);
    }

    #[test]
    fn default_history_records_the_providing_release_without_rewriting_older_records() {
        let current = release_app("release-host-provenance");
        let seeded = reconcile_builtin_with_host(&current, &current, None, None, UPDATE_TIME, 0,
            Some("0.4.0"), Some("old-revision")).unwrap();
        let first = crate::persistence::load_version(&current.id, &seeded.baseline.stamp).unwrap().0;
        assert_eq!(first.host_version.as_deref(), Some("0.4.0"));
        assert_eq!(first.host_revision.as_deref(), Some("old-revision"));
        let repeated = reconcile_builtin_with_host(&seeded.manifest, &current, Some(&seeded.baseline), None,
            UPDATE_TIME + 1, 0, Some("0.4.1"), Some("new-revision")).unwrap();
        assert_eq!(repeated.baseline, seeded.baseline);
        assert_eq!(crate::persistence::load_version(&current.id, &seeded.baseline.stamp).unwrap().0.host_version, first.host_version);
        let mut newer = current.clone(); newer.description = "New release description".into();
        let updated = reconcile_builtin_with_host(&seeded.manifest, &newer, Some(&seeded.baseline), None,
            UPDATE_TIME + 2, 0, Some("0.5.0"), Some("next-revision")).unwrap();
        let latest = crate::persistence::load_version(&current.id, &updated.baseline.stamp).unwrap().0;
        assert_eq!(latest.host_version.as_deref(), Some("0.5.0"));
        assert_eq!(latest.host_revision.as_deref(), Some("next-revision"));
        crate::persistence::remove_user_app(&current.id);
    }

    #[test]
    fn untouched_defaults_adopt_a_new_release_and_preserve_default_lineage() {
        let initial = release_app("release-untouched");
        let seeded = reconcile_builtin(&initial, &initial, None, None, UPDATE_TIME, 0).unwrap();
        let mut newer = initial.clone();
        newer.source = "View{ upgraded_default }".into();
        newer.description = "Updated description".into();
        let updated = reconcile_builtin(&seeded.manifest, &newer, Some(&seeded.baseline), None, UPDATE_TIME, 0).unwrap();
        assert!(updated.adopted_default);
        assert!(updated.available_update.is_none());
        assert!(matches_default(&updated.manifest, &newer));
        let stock = crate::persistence::load_version(&initial.id, &updated.baseline.stamp).unwrap().0;
        assert_eq!(stock.parent.as_deref(), Some(seeded.baseline.stamp.as_str()));
        assert_eq!(stock.at_unix, UPDATE_TIME);
        assert_eq!(stock.description.as_deref(), Some("Updated description"));
        crate::persistence::remove_user_app(&initial.id);
    }

    #[test]
    fn untouched_older_defaults_adopt_narrowed_declarations_that_limit_existing_group_grants() {
        let mut initial = release_app("release-narrow-capabilities");
        initial.capabilities.clear();
        let seeded = reconcile_builtin(&initial, &initial, None, None, UPDATE_TIME, 0).unwrap();
        let mut newer = initial.clone();
        newer.source = "// permissions: matrix-room-info, matrix.room.info.read\nView{ original_default }".into();
        newer.capabilities = vec!["matrix.room.info.read".into()];
        let updated = reconcile_builtin(&seeded.manifest, &newer, Some(&seeded.baseline), None, UPDATE_TIME + 1, 0).unwrap();
        assert!(updated.adopted_default);
        assert!(updated.available_update.is_none());
        assert_eq!(updated.manifest.source, newer.source);
        assert_eq!(updated.manifest.capabilities, newer.capabilities);
        let details = crate::capabilities::by_id("matrix.room.info.read").unwrap();
        let other = crate::capabilities::by_id("matrix.room.power_levels.read").unwrap();
        assert!(updated.manifest.declares_capability(details));
        assert!(!updated.manifest.declares_capability(other));
        // A saved group grant remains subject to the current declarations.
        let mut permissions = crate::permissions::PermissionStore::default();
        permissions.set(&initial.id, crate::permissions::Permission::MatrixRoomInfo, crate::permissions::GrantState::Granted);
        assert_eq!(permissions.effective_capability(&updated.manifest, other), crate::permissions::Effective::Undeclared);
        let archived = crate::persistence::load_version(&initial.id, &updated.baseline.stamp).unwrap().0;
        assert_eq!(archived.capabilities, newer.capabilities);
        crate::persistence::remove_user_app(&initial.id);
    }

    #[test]
    fn customized_apps_keep_their_branch_and_offer_each_latest_default() {
        let initial = release_app("release-customized");
        let seeded = reconcile_builtin(&initial, &initial, None, None, UPDATE_TIME, 0).unwrap();
        let mut custom = seeded.manifest.clone();
        custom.source = "View{ customized }".into();
        let manual = crate::versions::new_version(&custom, VersionOrigin::Manual, "Customized", custom.current_version.as_deref(), UPDATE_TIME, 0);
        custom.current_version = Some(crate::persistence::append_version(&custom, manual).unwrap());
        let custom_stamp = custom.current_version.clone();
        let mut newer = initial.clone();
        newer.source = "View{ new_default }".into();
        let updated = reconcile_builtin(&custom, &newer, Some(&seeded.baseline), None, UPDATE_TIME + 1, 0).unwrap();
        assert!(!updated.adopted_default);
        assert_eq!(updated.manifest.source, custom.source);
        assert_eq!(updated.manifest.current_version, custom_stamp);
        assert_eq!(updated.available_update.as_deref(), Some(updated.baseline.stamp.as_str()));
        let repeated = reconcile_builtin(&custom, &newer, Some(&updated.baseline), updated.available_update.as_deref(), UPDATE_TIME + 2, 0).unwrap();
        assert_eq!(repeated.baseline, updated.baseline);
        assert_eq!(crate::persistence::list_versions(&custom.id).len(), 3);
        newer.source = "View{ next_default }".into();
        let next = reconcile_builtin(&custom, &newer, Some(&updated.baseline), updated.available_update.as_deref(), UPDATE_TIME + 3, 0).unwrap();
        let stock = crate::persistence::load_version(&custom.id, &next.baseline.stamp).unwrap().0;
        assert_eq!(stock.parent.as_deref(), Some(updated.baseline.stamp.as_str()));
        assert_eq!(next.manifest.current_version, custom_stamp);
        let accepted = newer.clone();
        let cleared = reconcile_builtin(&accepted, &newer, Some(&next.baseline), next.available_update.as_deref(), UPDATE_TIME + 4, 0).unwrap();
        assert!(cleared.available_update.is_none());
        crate::persistence::remove_user_app(&custom.id);
    }

    #[test]
    fn unknown_modified_apps_preserve_code_and_offer_the_current_default() {
        let initial = release_app("release-unknown-copy");
        let mut custom = initial.clone();
        custom.source = "View{ unknown_modification }".into();
        let offered = reconcile_builtin(&custom, &initial, None, None, UPDATE_TIME, 0).unwrap();
        assert_eq!(offered.manifest.source, custom.source);
        assert!(offered.available_update.is_some());
        let current = crate::persistence::load_version(&custom.id, offered.manifest.current_version.as_deref().unwrap()).unwrap().0;
        assert_eq!(current.origin, VersionOrigin::Legacy);
        assert_eq!(current.at_unix, 0);
        assert!(current.actor.is_none());
        let default = crate::persistence::load_version(&custom.id, &offered.baseline.stamp).unwrap().0;
        assert!(default.parent.is_none());
        assert_eq!(default.at_unix, 0);
        let repeated = reconcile_builtin(&offered.manifest, &initial, Some(&offered.baseline), offered.available_update.as_deref(), UPDATE_TIME + 1, 0).unwrap();
        assert_eq!(repeated.available_update, offered.available_update);
        assert_eq!(crate::persistence::list_versions(&custom.id).len(), 2);
        crate::persistence::remove_user_app(&custom.id);
    }

    #[test]
    fn declaration_migrations_do_not_mistake_untouched_defaults_for_customizations() {
        let initial = release_app("release-declaration-migration");
        let seeded = reconcile_builtin(&initial, &initial, None, None, UPDATE_TIME, 0).unwrap();
        let mut newer = initial.clone();
        newer.permissions.push("network".into());
        newer.permission_reasons.insert("network".into(), "Updated default needs the network.".into());
        newer.normalize_permissions();
        let mut loaded = seeded.manifest.clone();
        migrate_default_declarations(&mut loaded, &newer);
        let adopted = reconcile_builtin(&loaded, &newer, Some(&seeded.baseline), None, UPDATE_TIME, 0).unwrap();
        assert!(adopted.adopted_default);
        assert!(matches_default(&adopted.manifest, &newer));
        assert!(adopted.available_update.is_none());
        crate::persistence::remove_user_app(&initial.id);
    }

    #[test]
    fn full_default_fingerprints_detect_metadata_and_widget_changes() {
        let initial = release_app("release-fingerprint");
        let fingerprint = builtin_fingerprint(&initial).unwrap();
        let mut local_state = initial.clone();
        local_state.id = "other-installed-id".into();
        local_state.scope = crate::manifest::A2AppScope::Room { room_id: "!room:test".into() };
        local_state.current_version = Some("any".into());
        assert_eq!(builtin_fingerprint(&local_state).unwrap(), fingerprint);
        let mut changes = Vec::new();
        let mut changed = initial.clone(); changed.name.push('!'); changes.push(changed);
        let mut changed = initial.clone(); changed.icon = "X".into(); changes.push(changed);
        let mut changed = initial.clone(); changed.tint ^= 1; changes.push(changed);
        let mut changed = initial.clone(); changed.description.push('!'); changes.push(changed);
        let mut changed = initial.clone(); changed.source.push('!'); changes.push(changed);
        let mut changed = initial.clone(); changed.permissions.push("network".into()); changes.push(changed);
        let mut changed = initial.clone(); changed.permission_reasons.clear(); changes.push(changed);
        let mut changed = initial.clone(); changed.capabilities.push("room.info".into()); changes.push(changed);
        let mut changed = initial.clone(); changed.shortcuts.push("Action".into()); changes.push(changed);
        let mut changed = initial.clone(); changed.widget = Some(crate::manifest::WidgetManifest {
            source: "Widget{}".into(), default_span: (2, 2), min_span: (1, 1),
        }); changes.push(changed);
        for changed in changes { assert!(!matches_default(&initial, &changed)); }
    }

    #[test]
    fn older_local_stock_pointers_auto_adopt_but_imported_stock_claims_do_not() {
        let initial = release_app("release-pretracking-local");
        let seeded = reconcile_builtin(&initial, &initial, None, None, UPDATE_TIME, 0).unwrap();
        let mut newer = initial.clone(); newer.source = "View{ new_release }".into();
        let adopted = reconcile_builtin(&seeded.manifest, &newer, None, None, UPDATE_TIME, 0).unwrap();
        assert!(adopted.adopted_default);
        assert!(adopted.available_update.is_none());
        crate::persistence::remove_user_app(&initial.id);
        let mut untrusted = initial.clone(); untrusted.id = "release-imported-stock".into();
        let mut version = crate::versions::new_version(&untrusted, VersionOrigin::Stock, "Claimed stock", None, UPDATE_TIME, 0);
        version.imported = true;
        untrusted.current_version = Some(crate::persistence::append_version(&untrusted, version).unwrap());
        newer.id = untrusted.id.clone();
        let offered = reconcile_builtin(&untrusted, &newer, None, None, UPDATE_TIME, 0).unwrap();
        assert_eq!(offered.manifest.source, untrusted.source);
        assert!(offered.available_update.is_some());
        crate::persistence::remove_user_app(&untrusted.id);
    }

    #[test]
    fn corrupt_baseline_tracking_preserves_customizations_and_offers_a_default() {
        let initial = release_app("release-corrupt-tracking");
        let seeded = reconcile_builtin(&initial, &initial, None, None, UPDATE_TIME, 0).unwrap();
        let mut customized = seeded.manifest.clone();
        customized.name = "My custom title".into();
        let damaged = BuiltinBaseline { fingerprint: "not-the-recorded-fingerprint".into(), stamp: seeded.baseline.stamp.clone() };
        let repaired = reconcile_builtin(&customized, &initial, Some(&damaged), None, UPDATE_TIME, 0).unwrap();
        assert_eq!(repaired.manifest.name, customized.name);
        assert_eq!(repaired.available_update.as_deref(), Some(seeded.baseline.stamp.as_str()));
        let mut missing = repaired.baseline.clone(); missing.stamp = "missing-stock-snapshot".into();
        let repaired = reconcile_builtin(&repaired.manifest, &initial, Some(&missing), None, UPDATE_TIME + 1, 0).unwrap();
        assert_eq!(repaired.manifest.name, customized.name);
        assert!(repaired.available_update.is_some());
        assert_eq!(repaired.baseline.stamp, seeded.baseline.stamp);
        crate::persistence::export_history(&initial.id).unwrap();
        crate::persistence::remove_user_app(&initial.id);
    }

    #[test]
    fn a_release_rollback_records_a_new_default_event_instead_of_reusing_old_history() {
        let original = release_app("release-rollback");
        let seeded = reconcile_builtin(&original, &original, None, None, UPDATE_TIME, 0).unwrap();
        let mut newer = original.clone(); newer.source = "View{ intermediate_release }".into();
        let adopted = reconcile_builtin(&seeded.manifest, &newer, Some(&seeded.baseline), None, UPDATE_TIME, 0).unwrap();
        let rollback = reconcile_builtin(&adopted.manifest, &original, Some(&adopted.baseline), None, UPDATE_TIME + 1, 0).unwrap();
        assert!(rollback.adopted_default);
        assert_ne!(rollback.baseline.stamp, seeded.baseline.stamp);
        let event = crate::persistence::load_version(&original.id, &rollback.baseline.stamp).unwrap().0;
        assert_eq!(event.parent.as_deref(), Some(adopted.baseline.stamp.as_str()));
        assert_eq!(event.at_unix, UPDATE_TIME + 1);
        crate::persistence::remove_user_app(&original.id);
    }

    /// The hardcoded catalog must agree with each app's own `.splash` header,
    /// or the prompt would show different reasons than the app declares.
    #[test]
    fn catalog_matches_the_splash_headers() {
        let apps = builtin_apps();
        assert_eq!(apps.len(), 20);
        for m in &apps {
            assert!(m.builtin);
            assert!(m.widget.is_none());
            let h = crate::header::parse_app_header(&m.source);
            assert_eq!(h.name.as_deref(), Some(m.name.as_str()), "{}", m.id);
            assert_eq!(h.tint, Some(m.tint), "{}", m.id);
            assert!(!m.description.is_empty(), "{} has no description", m.id);
            assert_eq!(h.permissions, m.permissions, "{}", m.id);
            assert_eq!(h.permission_reasons, m.permission_reasons, "{}", m.id);
            assert_eq!(h.capabilities, m.capabilities, "{}", m.id);
            assert!(!m.capabilities.is_empty(), "{} must narrow its groups to the abilities it uses", m.id);
            for permission in &m.permissions {
                let permission = crate::permissions::Permission::from_str(permission).unwrap();
                assert!(m.capabilities.iter().any(|id|
                    crate::capabilities::by_id(id).unwrap().group == Some(permission)),
                    "{} leaves {permission:?} unrestricted", m.id);
            }
        }
    }

    #[test]
    fn built_ins_run_where_they_say() {
        use crate::manifest::RunsIn;
        let apps = builtin_apps();
        let runs_in = |id: &str| apps.iter().find(|a| a.id == id).unwrap().runs_in();
        assert_eq!(runs_in("room-peek"), RunsIn::Room);
        assert_eq!(runs_in("search"), RunsIn::Room);
        assert_eq!(runs_in("inbox"), RunsIn::Rooms);
        assert_eq!(runs_in("spaces"), RunsIn::Spaces);
        assert_eq!(runs_in("account"), RunsIn::Account);
        assert_eq!(runs_in("inspector"), RunsIn::Account);
        assert_eq!(runs_in("public-web"), RunsIn::Account);
    }

    #[test]
    fn background_contexts_preserve_room_requirements_and_bound_scopes() {
        let reminder = stock("reminder").unwrap();
        assert!(reminder.can_run_background_in_context(None, false));
        assert!(reminder.can_run_background_in_context(Some("!room:test"), false));
        assert!(reminder.can_run_background_in_context(Some("!space:test"), true));
        for id in ["website-watch", "keyword-alert"] {
            let app = stock(id).unwrap();
            assert!(!app.can_run_background_in_context(None, false));
            assert!(!app.can_run_background_in_context(Some("!space:test"), true));
            assert!(app.can_run_background_in_context(Some("!room:test"), false));
        }
        let mut bound = reminder;
        bound.scope = crate::manifest::A2AppScope::Room { room_id: "!original:test".into() };
        assert!(bound.can_run_background_in_context(Some("!original:test"), false));
        assert!(!bound.can_run_background_in_context(None, false));
        assert!(!bound.can_run_background_in_context(Some("!other:test"), false));
        assert!(!stock("room-peek").unwrap().can_run_background_in_context(Some("!room:test"), false));
    }

    /// Every stock app must parse with the real Splash parser, or it would
    /// launch to an empty pane.
    #[test]
    fn builtin_sources_parse() {
        use makepad_widgets::makepad_script::{parser::ScriptParser, tokenizer::ScriptTokenizer, ScriptVmBase};

        fn parse_errors(base: &mut ScriptVmBase, source: &str) -> Vec<String> {
            let mut tokenizer = ScriptTokenizer::default();
            tokenizer.tokenize(source, &mut base.heap);
            let mut parser = ScriptParser::default();
            parser.parse(&tokenizer, "builtin", (0, 0), &[]);
            parser.parse_errors
        }

        let mut base = ScriptVmBase::new();
        for m in builtin_apps() {
            let errors = parse_errors(&mut base, &m.source);
            assert!(errors.is_empty(), "{}: {errors:?}", m.id);
        }
        // A missing property value is a parse error, so the check has teeth.
        assert!(!parse_errors(&mut base, "View{ width: }").is_empty());
    }
}
