//! Mini-app manifests and the in-memory registry of installed apps.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

/// A stable identifier for an installed mini-app, e.g. `"roll-call"`.
pub type MiniAppId = String;

/// Everything the host knows about one installed mini-app.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MiniAppManifest {
    pub id: MiniAppId,
    /// Display name shown under the icon.
    pub name: String,
    /// Emoji drawn as the icon glyph (rendered via the built-in emoji font).
    pub icon: String,
    /// Tint color of the icon tile, as 0xRRGGBB.
    pub tint: u32,
    /// The Splash source code of the app itself.
    pub source: String,
    /// Legacy pre-permissions field; normalized into `permissions` at every
    /// load and kept in sync so older builds still read exported apps right.
    pub allow_net: bool,
    /// Permission ids this app DECLARES it may use. Undeclared capabilities
    /// are ungrantable. Grant state is the user's, lives in the host's
    /// permission store, and never travels with the app.
    #[serde(default)]
    pub permissions: Vec<String>,
    /// Why this app wants each permission, in ITS words — iOS's usage strings.
    /// Keyed by permission id; missing entries fall back to the host's
    /// generic description. Shown on the prompt, always attributed to the app
    /// so a persuasive string can't pose as the system.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub permission_reasons: BTreeMap<String, String>,
    /// Individual capability ids the app declares, narrowing a declared
    /// permission group to just these. Empty means "everything in the
    /// groups declared above".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// Pre-installed apps cannot be uninstalled.
    pub builtin: bool,
    /// A widget-form Splash script some host_launcher bundles carry. Kept for
    /// bundle compat; Robrix never runs widget scripts.
    pub widget: Option<WidgetManifest>,
    /// Quick-action labels from host_launcher bundles; bundle compat only.
    #[serde(default)]
    pub shortcuts: Vec<String>,
    /// Where this app lives: the whole account, or attached to one room.
    #[serde(default)]
    pub scope: A2AppScope,
    /// The stamp of the version in `versions/` this working copy currently IS.
    #[serde(default)]
    pub current_version: Option<String>,
}

impl MiniAppManifest {
    /// Reconciles the legacy `allow_net` flag with the `permissions` list, in
    /// both directions: an old export's `allow_net: true` becomes a declared
    /// `network`, and `allow_net` mirrors the declaration so downgrades and
    /// old readers keep working. Call at every point a manifest enters the
    /// system (builtins, disk load, import, generation).
    pub fn normalize_permissions(&mut self) {
        if self.allow_net && !self.declares(crate::permissions::Permission::Network) {
            self.permissions.push("network".to_string());
        }
        // Drop unknown and duplicate ids at the door: an id this build can't
        // grant is noise in every list, and a repeat would burn a fixed UI row.
        let mut seen: Vec<String> = Vec::new();
        for p in std::mem::take(&mut self.permissions) {
            if crate::permissions::Permission::from_str(&p).is_some() && !seen.contains(&p) {
                seen.push(p);
            }
        }
        self.permissions = seen;
        self.permission_reasons
            .retain(|id, _| self.permissions.iter().any(|p| p == id));
        // A declared capability implies its group; unknown ids are dropped
        // like unknown permissions.
        let mut caps = Vec::new();
        for id in std::mem::take(&mut self.capabilities) {
            let Some(cap) = crate::capabilities::by_id(&id) else { continue };
            if caps.contains(&id) {
                continue;
            }
            if let Some(group) = cap.group
                && !self.declares(group)
            {
                self.permissions.push(group.as_str().to_string());
            }
            caps.push(id);
        }
        self.capabilities = caps;
        self.allow_net = self.declares(crate::permissions::Permission::Network);
    }

    /// Whether this app declares a permission (the precondition for it ever
    /// being granted).
    pub fn declares(&self, perm: crate::permissions::Permission) -> bool {
        self.permissions.iter().any(|p| p == perm.as_str())
    }

    /// Whether the app may ever use this capability: its group is declared,
    /// and if the app narrowed that group to specific capabilities, this is
    /// one of them. Ungated plumbing is always declared.
    pub fn declares_capability(&self, cap: &crate::capabilities::Capability) -> bool {
        let Some(group) = cap.group else { return true };
        if !self.declares(group) {
            return false;
        }
        let narrowed = self.capabilities.iter().any(|id| {
            crate::capabilities::by_id(id).is_some_and(|c| c.group == Some(group))
        });
        !narrowed || self.capabilities.iter().any(|id| id == cap.id)
    }

    /// The app's own explanation for wanting a permission, if it gave one.
    pub fn reason_for(&self, perm: crate::permissions::Permission) -> Option<&str> {
        self.permission_reasons
            .get(perm.as_str())
            .map(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
    }

    /// Adds a declaration (the capability editor for apps the user owns).
    /// No-op when already declared; keeps `allow_net` in step.
    pub fn declare(&mut self, perm: crate::permissions::Permission) {
        if !self.declares(perm) {
            self.permissions.push(perm.as_str().to_string());
            self.normalize_permissions();
        }
    }

    /// Removes a declaration, which also makes the capability ungrantable.
    pub fn undeclare(&mut self, perm: crate::permissions::Permission) {
        self.permissions.retain(|p| p != perm.as_str());
        self.permission_reasons.remove(perm.as_str());
        self.normalize_permissions();
    }
}

/// `base` with a rewritten `source`: the header may restyle it, and declarations
/// only ever grow, since the user granted against the existing ones.
pub fn rewritten(base: &MiniAppManifest, source: String) -> MiniAppManifest {
    let header = crate::header::parse_app_header(&source);
    MiniAppManifest {
        id: base.id.clone(),
        name: header.name.unwrap_or_else(|| base.name.clone()),
        icon: header.icon.unwrap_or_else(|| base.icon.clone()),
        tint: header.tint.unwrap_or(base.tint),
        source,
        allow_net: base.allow_net,
        permissions: union_permissions(&base.permissions, &header.permissions),
        permission_reasons: {
            let mut r = base.permission_reasons.clone();
            r.extend(header.permission_reasons);
            r
        },
        // A narrowed id lands only for a group new to the app or one already
        // narrowed; narrowing a whole granted group would drop abilities.
        capabilities: {
            let mut caps = base.capabilities.clone();
            for id in &header.capabilities {
                let Some(group) = crate::capabilities::by_id(id).and_then(|c| c.group) else { continue };
                let new_group = !base.permissions.iter().any(|p| p == group.as_str());
                let narrowed = caps.iter().any(|c| {
                    crate::capabilities::by_id(c).is_some_and(|c| c.group == Some(group))
                });
                if (new_group || narrowed) && !caps.contains(id) {
                    caps.push(id.clone());
                }
            }
            caps
        },
        builtin: base.builtin,
        // Robrix never runs widget scripts, so nothing to carry over.
        widget: None,
        shortcuts: Vec::new(),
        scope: base.scope.clone(),
        current_version: base.current_version.clone(),
    }
}

/// Base declarations plus anything the rewrite added, order preserved.
pub fn union_permissions(base: &[String], added: &[String]) -> Vec<String> {
    let mut out = base.to_vec();
    for p in added {
        if !out.contains(p) {
            out.push(p.clone());
        }
    }
    out
}

/// A widget provided by a mini-app: a separate, smaller Splash script.
/// Bundle compat only; Robrix never runs it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WidgetManifest {
    /// The Splash source code of the widget form of the app.
    pub source: String,
    /// Default size in grid cells (cols, rows).
    pub default_span: (u8, u8),
    /// Minimum size in grid cells.
    pub min_span: (u8, u8),
}

/// What a mini-app is scoped to. An account app opens standalone; a room app
/// belongs to one room and gets that room's services.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum A2AppScope {
    #[default]
    Account,
    Room { room_id: String },
}

/// The full app registry: every installed app, in a stable order.
#[derive(Default)]
pub struct AppRegistry {
    apps: Vec<MiniAppManifest>,
    index: HashMap<MiniAppId, usize>,
}

impl AppRegistry {
    pub fn new(apps: Vec<MiniAppManifest>) -> Self {
        let mut registry = Self::default();
        for app in apps {
            registry.insert(app);
        }
        registry
    }

    pub fn insert(&mut self, app: MiniAppManifest) {
        if let Some(&i) = self.index.get(&app.id) {
            self.apps[i] = app;
        } else {
            self.index.insert(app.id.clone(), self.apps.len());
            self.apps.push(app);
        }
    }

    pub fn remove(&mut self, id: &str) -> Option<MiniAppManifest> {
        let i = self.index.remove(id)?;
        let app = self.apps.remove(i);
        // Reindex everything after the removed entry.
        for (j, a) in self.apps.iter().enumerate().skip(i) {
            self.index.insert(a.id.clone(), j);
        }
        Some(app)
    }

    pub fn get(&self, id: &str) -> Option<&MiniAppManifest> {
        self.index.get(id).map(|&i| &self.apps[i])
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut MiniAppManifest> {
        self.index.get(id).map(|&i| &mut self.apps[i])
    }

    pub fn contains(&self, id: &str) -> bool {
        self.index.contains_key(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &MiniAppManifest> {
        self.apps.iter()
    }

    pub fn len(&self) -> usize {
        self.apps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.apps.is_empty()
    }
}

/// Separator between an app id and its room in a host instance tag. App ids
/// are slug-safe and room ids never contain '@', so the split is unambiguous.
pub const INSTANCE_TAG_SEP: char = '@';

/// The host tag for one running instance of an app.
pub fn instance_tag(app_id: &str, room_id: Option<&str>) -> String {
    match room_id {
        Some(room) => format!("{app_id}{INSTANCE_TAG_SEP}{room}"),
        None => app_id.to_string(),
    }
}

/// Splits an instance tag back into (app_id, room_id).
pub fn split_instance_tag(tag: &str) -> (&str, Option<&str>) {
    match tag.split_once(INSTANCE_TAG_SEP) {
        Some((app, room)) => (app, Some(room)),
        None => (tag, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> MiniAppManifest {
        MiniAppManifest {
            id: "sunrise".into(),
            name: "Sunrise".into(),
            icon: "🌅".into(),
            tint: 0x112233,
            source: "View{}".into(),
            allow_net: true,
            permissions: vec!["network".into(), "location".into()],
            permission_reasons: [("location".to_string(), "Uses your city.".to_string())].into(),
            capabilities: Vec::new(),
            builtin: false,
            widget: None,
            shortcuts: vec!["Start".into()],
            scope: A2AppScope::Room { room_id: "!r:example.org".into() },
            current_version: Some("20260724-153204".into()),
        }
    }

    /// A refine may ADD what the rewrite needs but never drops a declaration
    /// the user has already granted against.
    #[test]
    fn refine_unions_declarations() {
        let base = vec!["network".to_string(), "location".to_string()];
        let added = vec!["location".to_string(), "clipboard-write".to_string()];
        assert_eq!(
            union_permissions(&base, &added),
            vec![
                "network".to_string(),
                "location".to_string(),
                "clipboard-write".to_string()
            ]
        );
    }

    #[test]
    fn a_rewrite_keeps_identity_and_unions_the_headers_declarations() {
        let source = "// name: Sunrise II\n\
                      // permissions: clipboard-write\n\
                      // why-clipboard-write: Copies the time.\n\
                      View{}";
        let m = rewritten(&base(), source.to_string());
        assert_eq!(m.id, "sunrise");
        assert_eq!(m.name, "Sunrise II");
        // No icon/tint in the header, so base's stay.
        assert_eq!(m.icon, "🌅");
        assert_eq!(m.tint, 0x112233);
        assert_eq!(m.source, source);
        assert_eq!(
            m.permissions,
            vec!["network".to_string(), "location".to_string(), "clipboard-write".to_string()]
        );
        assert_eq!(m.permission_reasons.get("location").unwrap(), "Uses your city.");
        assert_eq!(m.permission_reasons.get("clipboard-write").unwrap(), "Copies the time.");
        assert_eq!(m.scope, A2AppScope::Room { room_id: "!r:example.org".into() });
        assert_eq!(m.current_version.as_deref(), Some("20260724-153204"));
        assert!(m.widget.is_none());
        assert!(m.shortcuts.is_empty());
    }
}
