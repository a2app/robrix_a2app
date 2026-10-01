//! Version history for mini-apps.
//!
//! Every state an app has been in is a version, appended and never pruned.
//! The working copy points at the one it currently IS (`current_version`),
//! and each version points at the one it was edited from. They live beside
//! the app's code:
//!
//! ```text
//! apps/<id>/versions/20260724-153204.splash   the source as it was
//! apps/<id>/versions/20260724-153204.json     when, why, and the manifest fields
//! ```
//!
//! The stamp is local wall-clock time. The `.splash` is written first and the
//! `.json` (which listing keys off) last, so a crash mid-write leaves an
//! ignored orphan rather than a version with no code.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::manifest::{A2AppScope, MiniAppManifest, WidgetManifest};

/// Where a version came from. `Legacy` means its original origin is unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum VersionOrigin {
    Ai,
    Manual,
    Import,
    Stock,
    Copy,
    Restore,
    #[default]
    Legacy,
}

impl VersionOrigin {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ai => "AI",
            Self::Manual => "Edited by hand",
            Self::Import => "Imported",
            Self::Stock => "Stock",
            Self::Copy => "Copied",
            Self::Restore => "Restored",
            Self::Legacy => "Earlier",
        }
    }
}

/// The person responsible for a local change, or claimed by shared history.
/// Missing attribution stays unknown; old records never inherit the importer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionActor {
    pub user_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

impl VersionActor {
    pub fn label(&self) -> String {
        match self.display_name.as_deref().filter(|name| !name.trim().is_empty()) {
            Some(name) => format!("{name} ({})", self.user_id),
            None => self.user_id.clone(),
        }
    }
}

/// How this installation acquired an app. Room senders identify the transfer,
/// independently of whoever the shared history says wrote the code.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AcquisitionSource {
    BuiltIn { app_id: String },
    File {
        file_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    Clipboard,
    Generated {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        room_id: Option<String>,
    },
    RoomAttachment {
        room_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media_uri: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shared_at_unix: Option<u64>,
        file_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sender: Option<VersionActor>,
    },
    Copy { app_id: String },
}

impl AcquisitionSource {
    pub fn label(&self) -> String {
        match self {
            Self::BuiltIn { app_id } => format!("Built-in app: {app_id}"),
            Self::File { file_name, path } => match path {
                Some(path) => format!("File: {file_name} · {path}"),
                None => format!("File: {file_name}"),
            },
            Self::Clipboard => "Clipboard".into(),
            Self::Generated { model, room_id } => {
                let model = model.as_ref().map(|model| format!(" · model {model}")).unwrap_or_default();
                let room = room_id.as_ref().map(|room| format!(" · room {room}")).unwrap_or_default();
                format!("Generated locally{model}{room}")
            }
            Self::RoomAttachment { room_id, event_id, media_uri, shared_at_unix, file_name, sender } => {
                let sender = sender.as_ref().map(|actor| format!(" · shared by {}", actor.label())).unwrap_or_default();
                let event = event_id.as_ref().map(|id| format!(" · message {id}")).unwrap_or_default();
                let media = media_uri.as_ref().map(|uri| format!(" · {uri}")).unwrap_or_default();
                let shared = shared_at_unix.as_ref().map(|at| {
                    let (year, month, day, hour, minute, second) = civil_from_unix(*at, 0);
                    format!(" · shared {year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
                }).unwrap_or_default();
                format!("{file_name} · room {room_id}{sender}{event}{media}{shared}")
            }
            Self::Copy { app_id } => format!("Copied from: {app_id}"),
        }
    }
}

/// One entry in an app's history: the whole manifest minus its source,
/// which sits in the sibling `.splash`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppVersion {
    /// Filename key, e.g. `20260724-153204` (local time, sortable).
    pub stamp: String,
    /// Seconds since the unix epoch, for ordering across DST/tz moves.
    /// Zero means the date was not recorded.
    pub at_unix: u64,
    /// The request that produced this version, or a marker like "Original".
    pub note: String,
    pub name: String,
    pub icon: String,
    pub tint: u32,
    /// Absent in older records; their source header remains the fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub origin: VersionOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<VersionActor>,
    /// The Robrix build that recorded this change or supplied this default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acquired_from: Option<AcquisitionSource>,
    /// The attribution came from an imported bundle and is not authenticated.
    #[serde(default)]
    pub imported: bool,
    /// The stamp this version was edited from.
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub allow_net: bool,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub permission_reasons: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub shortcuts: Vec<String>,
    #[serde(default)]
    pub scope: A2AppScope,
    /// Older version files did not record the optional widget's source.
    #[serde(default)]
    pub widget_recorded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widget: Option<WidgetManifest>,
}

/// A portable version including its archived code, suitable for sharing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VersionSnapshot {
    pub version: AppVersion,
    pub source: String,
}

impl VersionSnapshot {
    /// Scope and installed identity are local; the portable app fields agree.
    pub fn matches_manifest(&self, manifest: &MiniAppManifest) -> bool {
        let version = &self.version;
        let description = version.description.clone().unwrap_or_else(||
            crate::header::parse_app_header(&self.source).description.unwrap_or_default());
        self.source == manifest.source && version.name == manifest.name
            && version.icon == manifest.icon && version.tint == manifest.tint
            && description == manifest.description
            && version.allow_net == manifest.allow_net && version.permissions == manifest.permissions
            && version.permission_reasons == manifest.permission_reasons
            && version.capabilities == manifest.capabilities && version.shortcuts == manifest.shortcuts
            && if version.widget_recorded { version.widget == manifest.widget } else { manifest.widget.is_none() }
    }
}

impl AppVersion {
    /// The working copy this version describes: `base` with this version's
    /// identity, declarations, and `source`. Id, builtin, and scope stay base's.
    pub fn apply_to(&self, base: &MiniAppManifest, source: String) -> MiniAppManifest {
        let mut manifest = MiniAppManifest {
            description: self.description.clone().unwrap_or_else(|| crate::header::parse_app_header(&source).description
                .unwrap_or_else(|| base.description.clone())),
            source,
            name: self.name.clone(),
            icon: self.icon.clone(),
            tint: self.tint,
            current_version: Some(self.stamp.clone()),
            ..base.clone()
        };
        // Older Legacy files did not record declarations. Modern snapshots
        // record the full contract alongside the optional widget marker.
        if self.origin != VersionOrigin::Legacy || self.widget_recorded {
            manifest.allow_net = self.allow_net;
            manifest.permissions = self.permissions.clone();
            manifest.permission_reasons = self.permission_reasons.clone();
            manifest.capabilities = self.capabilities.clone();
            manifest.shortcuts = self.shortcuts.clone();
        }
        if self.widget_recorded {
            manifest.widget = self.widget.clone();
        }
        crate::builtin::union_stock_declarations(&mut manifest);
        manifest.normalize_permissions();
        manifest
    }
}

/// Y-M-D H:M:S in the local zone, from a unix timestamp + the local offset.
/// (`chrono` is only an optional dep here, and this is all the calendar math
/// we need: Howard Hinnant's civil-from-days, valid for any modern date.)
pub fn civil_from_unix(at_unix: u64, offset_secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let local = at_unix as i64 + offset_secs;
    let days = local.div_euclid(86_400);
    let secs_of_day = local.rem_euclid(86_400);

    // Shift the era so March is month 1 (leap day lands at the end of a year).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };

    (
        y,
        m,
        d,
        (secs_of_day / 3600) as u32,
        ((secs_of_day % 3600) / 60) as u32,
        (secs_of_day % 60) as u32,
    )
}

/// The filename key for a moment: `YYYYMMDD-HHMMSS` in local time.
pub fn stamp_for(at_unix: u64, offset_secs: i64) -> String {
    let (y, mo, d, h, mi, s) = civil_from_unix(at_unix, offset_secs);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// A short human label for a version row, e.g. `Jul 24, 15:32`.
pub fn label_for(at_unix: u64, offset_secs: i64) -> String {
    if at_unix == 0 || i64::try_from(at_unix).ok().and_then(|at| at.checked_add(offset_secs)).is_none() {
        return "Date not recorded".into();
    }
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (_y, mo, d, h, mi, _s) = civil_from_unix(at_unix, offset_secs);
    let month = MONTHS.get(mo.saturating_sub(1) as usize).copied().unwrap_or("???");
    format!("{month} {d}, {h:02}:{mi:02}")
}

/// Seconds since the unix epoch, now.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The version record for `manifest` as it stands right now. `parent` is the
/// stamp it was edited from.
pub fn new_version(
    manifest: &MiniAppManifest,
    origin: VersionOrigin,
    note: &str,
    parent: Option<&str>,
    at_unix: u64,
    offset_secs: i64,
) -> AppVersion {
    AppVersion {
        stamp: stamp_for(at_unix, offset_secs),
        at_unix,
        note: note.trim().to_string(),
        name: manifest.name.clone(),
        icon: manifest.icon.clone(),
        tint: manifest.tint,
        description: Some(manifest.description.clone()),
        origin,
        actor: None,
        host_version: None,
        host_revision: None,
        acquired_from: (origin == VersionOrigin::Stock)
            .then(|| AcquisitionSource::BuiltIn { app_id: manifest.id.clone() }),
        imported: false,
        parent: parent.map(str::to_string),
        allow_net: manifest.allow_net,
        permissions: manifest.permissions.clone(),
        permission_reasons: manifest.permission_reasons.clone(),
        capabilities: manifest.capabilities.clone(),
        shortcuts: manifest.shortcuts.clone(),
        scope: manifest.scope.clone(),
        widget_recorded: true,
        widget: manifest.widget.clone(),
    }
}

/// Compat shim for callers not yet passing an origin or parent.
pub fn version_of(manifest: &MiniAppManifest, note: &str, at_unix: u64, offset_secs: i64) -> AppVersion {
    new_version(manifest, VersionOrigin::Legacy, note, None, at_unix, offset_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_conversion_matches_known_dates() {
        // 2026-07-24 15:32:04 UTC = 1784907124.
        assert_eq!(civil_from_unix(1_784_907_124, 0), (2026, 7, 24, 15, 32, 4));
        // The unix epoch itself.
        assert_eq!(civil_from_unix(0, 0), (1970, 1, 1, 0, 0, 0));
        // A leap day.
        assert_eq!(civil_from_unix(1_709_164_800, 0), (2024, 2, 29, 0, 0, 0));
        // Negative offsets roll the date back across midnight.
        assert_eq!(civil_from_unix(1_709_164_800, -8 * 3600), (2024, 2, 28, 16, 0, 0));
    }

    #[test]
    fn stamps_are_sortable_and_labels_readable() {
        let earlier = stamp_for(1_784_907_124, 0);
        let later = stamp_for(1_784_907_124 + 3600, 0);
        assert_eq!(earlier, "20260724-153204");
        assert!(later > earlier, "stamps must sort chronologically as strings");
        assert_eq!(label_for(1_784_907_124, 0), "Jul 24, 15:32");
        assert_eq!(label_for(0, -8 * 3600), "Date not recorded");
        assert_eq!(label_for(u64::MAX, 0), "Date not recorded");
    }

    /// A pre-origin json file has none of the new fields and must still parse
    /// as a Legacy version.
    #[test]
    fn old_version_files_parse_as_legacy() {
        let v: AppVersion = serde_json::from_str(
            r#"{"stamp":"20260724-153204","at_unix":1784907124,"note":"x","name":"N","icon":"i","tint":7}"#,
        )
        .unwrap();
        assert_eq!(v.origin, VersionOrigin::Legacy);
        assert!(v.parent.is_none());
        assert!(v.permissions.is_empty());
        assert!(v.actor.is_none());
        assert!(v.host_version.is_none());
        assert!(v.host_revision.is_none());
        assert!(v.acquired_from.is_none());
        assert!(!v.imported);
    }

    #[test]
    fn file_provenance_keeps_the_download_location_and_stable_identity() {
        let source = AcquisitionSource::File {
            file_name: "app.splashapp".into(), path: Some("/Downloads/app.splashapp".into()),
        };
        let saved = serde_json::to_string(&source).unwrap();
        let restored: AcquisitionSource = serde_json::from_str(&saved).unwrap();
        assert_eq!(source, restored);
        assert!(restored.label().contains("/Downloads/app.splashapp"));
        let actor = VersionActor { user_id: "@alice:example.org".into(), display_name: Some("Alice".into()) };
        assert_eq!(actor.label(), "Alice (@alice:example.org)");
        let old: AcquisitionSource = serde_json::from_str(r#"{"kind":"file","file_name":"old.splashapp"}"#).unwrap();
        assert!(matches!(old, AcquisitionSource::File { path: None, .. }));
    }
}
