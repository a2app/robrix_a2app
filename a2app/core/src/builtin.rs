//! The manifests of the pre-installed mini-apps.
//!
//! Splash sources live in the `apps/` directory. They're baked into the binary,
//! but in a dev checkout we prefer reading them from disk so `.splash` edits
//! show up on the next app launch without a rebuild.

use crate::manifest::MiniAppManifest;

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
    let mut manifest = MiniAppManifest {
        id: id.to_string(),
        name: name.to_string(),
        icon: icon.to_string(),
        tint,
        source,
        allow_net: false,
        permissions: permissions_for(id),
        permission_reasons: reasons_for(id),
        capabilities: Vec::new(),
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
        "room-peek" => &["matrix-room-info", "matrix-room-read", "matrix-room-send", "robrix-navigation", "matrix-room-watch"],
        "roll-call" => &["matrix-profile", "matrix-room-send"],
        "room-info" => &["matrix-room-info"],
        "room-members" | "room-threads" => &["matrix-room-read", "robrix-navigation", "matrix-room-watch"],
        "search" => &["matrix-room-read", "matrix-rooms-list", "matrix-rooms-read", "robrix-navigation"],
        "watcher" => &["matrix-room-watch", "notifications", "matrix-room-send"],
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
        "room-peek" => &[
            ("matrix-room-info", "Shows this room's name and member count."),
            ("matrix-room-read", "Lists the latest messages in this room."),
            ("matrix-room-send", "Sends the message you type into this room."),
            ("robrix-navigation", "Jumps to a message you tap."),
            ("matrix-room-watch", "Shows new messages as they arrive."),
        ],
        "roll-call" => &[
            ("matrix-profile", "Shows who is rolling."),
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
            ("matrix-room-read", "Lists the discussion threads in this room."),
            ("robrix-navigation", "Opens a thread you tap."),
            ("matrix-room-watch", "Refreshes as new messages arrive."),
        ],
        "search" => &[
            ("matrix-room-read", "Searches the messages in this room."),
            ("matrix-rooms-list", "Lists your rooms so you can pick which to search."),
            ("matrix-rooms-read", "Searches messages across the rooms you pick."),
            ("robrix-navigation", "Jumps to a result you tap."),
        ],
        "watcher" => &[
            ("matrix-room-watch", "Sees new messages so it can match your rules."),
            ("notifications", "Tells you when a message matches a rule."),
            ("matrix-room-send", "Posts your reply when a rule says to."),
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
        app("room-peek", "Room Peek", "👀", 0x4A90D9, app_source!("room_peek.splash")),
        app("roll-call", "Roll Call", "🎲", 0x7C6CF0, app_source!("roll_call.splash")),
        app("room-info", "Room Info", "🏷", 0x2E86AB, app_source!("room_info.splash")),
        app("room-members", "Room Members", "👥", 0x6C8E3A, app_source!("room_members.splash")),
        app("room-pins", "Pinned Events", "📌", 0xC0533E, app_source!("room_pins.splash")),
        app("room-threads", "Room Threads", "🧵", 0x8A5CA8, app_source!("room_threads.splash")),
        app("search", "Search", "🔍", 0x0F88FE, app_source!("search.splash")),
        app("watcher", "Watcher", "👁", 0xD9822B, app_source!("watcher.splash")),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hardcoded catalog must agree with each app's own `.splash` header,
    /// or the prompt would show different reasons than the app declares.
    #[test]
    fn catalog_matches_the_splash_headers() {
        let apps = builtin_apps();
        assert_eq!(apps.len(), 8);
        for m in &apps {
            assert!(m.builtin);
            assert!(m.widget.is_none());
            let h = crate::header::parse_app_header(&m.source);
            assert_eq!(h.name.as_deref(), Some(m.name.as_str()), "{}", m.id);
            assert_eq!(h.tint, Some(m.tint), "{}", m.id);
            assert_eq!(h.permissions, m.permissions, "{}", m.id);
            assert_eq!(h.permission_reasons, m.permission_reasons, "{}", m.id);
        }
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
