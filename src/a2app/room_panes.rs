//! Mini-apps shown as room panes, either docked around a room's timeline or popped out.
//!
//! A pane only draws an app's instance, which lives in the instance registry,
//! so a pane can be parked (e.g., when its room is hidden) and shown again with its state intact.

use makepad_widgets::*;
use matrix_sdk::ruma::RoomId;

use a2app_core::layout::PaneSide as AppPaneSide;
use crate::room::room_pane::PaneSide;
use super::host_set::MiniAppHostAreaWidgetRefExt;
use super::instances::{self, InstanceKey, MiniAppInstanceAction, Surface};
use super::runtime::with_a2app;

fn key(room_id: &RoomId, app_id: &str) -> InstanceKey {
    (app_id.to_string(), Some(room_id.to_owned()))
}

/// Returns the given app's name, if it's installed.
pub fn app_name(app_id: &str) -> Option<String> {
    with_a2app(|state| state.registry.get(app_id).map(|manifest| manifest.name.clone())).flatten()
}

/// Returns the given app's icon glyph (e.g., an emoji), if it's installed.
pub fn app_glyph(app_id: &str) -> Option<String> {
    with_a2app(|state| state.registry.get(app_id).map(|manifest| manifest.icon.clone())).flatten()
}

/// Shows the room's instance of the app in the given host area, docked on `side` or popped out if `None`.
/// Only starts the app if `is_spawn_allowed`; returns whether it's now shown.
pub fn attach(
    cx: &mut Cx,
    host_area: &WidgetRef,
    surface_uid: WidgetUid,
    room_id: &RoomId,
    app_id: &str,
    side: Option<PaneSide>,
    is_spawn_allowed: bool,
) -> bool {
    let key = key(room_id, app_id);
    if instances::host_of(&key).is_none() {
        if !is_spawn_allowed {
            return false;
        }
        // The same checks as opening an app, as a restored pane may outlive a force stop.
        let manifest = with_a2app(|state| {
            (!state.permissions.is_restricted(app_id)).then(|| state.registry.get(app_id).cloned()).flatten()
        }).flatten();
        let Some(manifest) = manifest else { return false };
        let grants = a2app_core::permissions::snapshot_grants_for(app_id);
        if instances::ensure(cx, &key, &manifest, &grants).is_none() {
            return false;
        }
    }
    if let Some(side) = side {
        instances::set_side(&key, to_app_side(side));
    }
    let surface = if side.is_some() { Surface::Dock } else { Surface::Tab };
    let Some(host) = instances::adopt(cx, &key, surface_uid, surface) else { return false };
    host_area.as_mini_app_host_area().set_host(Some(host));
    true
}

/// Stops showing the app in the given host area, then either quits or parks its instance.
///
/// This doesn't change an instance that the given surface isn't showing.
pub fn detach(
    cx: &mut Cx,
    host_area: &WidgetRef,
    surface_uid: WidgetUid,
    room_id: &RoomId,
    app_id: &str,
    is_quitting: bool,
) {
    host_area.as_mini_app_host_area().set_host(None);
    let key = key(room_id, app_id);
    if instances::shown_by(&key) != Some(surface_uid) {
        return;
    }
    if is_quitting {
        quit(cx, &key);
    } else {
        instances::release(cx, &key, surface_uid);
    }
}

fn quit(cx: &mut Cx, key: &InstanceKey) {
    if instances::quit(cx, key) {
        cx.action(MiniAppInstanceAction::AppStopped(key.0.clone()));
    }
}

/// Returns whether the given surface is showing the app's instance for the given room.
pub fn is_shown_by(room_id: &RoomId, app_id: &str, surface_uid: WidgetUid) -> bool {
    instances::shown_by(&key(room_id, app_id)) == Some(surface_uid)
}

/// Returns whether the app is running in the given room.
pub fn is_running(room_id: &RoomId, app_id: &str) -> bool {
    instances::host_of(&key(room_id, app_id)).is_some()
}

/// Quits the app's instance for the given room if no surface is showing it,
/// e.g., when closing a popped-out pane that never showed it.
pub fn quit_if_parked(cx: &mut Cx, room_id: &RoomId, app_id: &str) {
    let key = key(room_id, app_id);
    if instances::host_of(&key).is_some() && instances::shown_by(&key).is_none() {
        quit(cx, &key);
    }
}

/// Returns the apps running in the given room that nothing shows, excluding background tasks.
pub fn parked_apps(room_id: &RoomId) -> Vec<String> {
    instances::parked_apps_in_room(room_id)
}

/// Tells the app that its docked pane moved to the given side.
pub fn note_side(room_id: &RoomId, app_id: &str, side: PaneSide) {
    let key = key(room_id, app_id);
    instances::set_side(&key, to_app_side(side));
    instances::note_hook(&key, live_id!(on_surface_changed));
}

/// Tells the app the size of the given host area, after it was drawn.
pub fn note_size(host_area: &WidgetRef, room_id: &RoomId, app_id: &str) {
    instances::note_size(&key(room_id, app_id), host_area.as_mini_app_host_area().last_size());
}

/// Parks every app that the given surface shows, e.g., when it's dropped.
pub fn release_all_shown_by(surface_uid: WidgetUid) {
    instances::release_owner_no_cx(surface_uid);
}

fn to_app_side(side: PaneSide) -> AppPaneSide {
    match side {
        PaneSide::Top => AppPaneSide::Top,
        PaneSide::Bottom => AppPaneSide::Bottom,
        PaneSide::Left => AppPaneSide::Left,
        PaneSide::Right => AppPaneSide::Right,
    }
}

/// Converts a side requested by an app into a pane side.
pub fn from_app_side(side: AppPaneSide) -> PaneSide {
    match side {
        AppPaneSide::Top => PaneSide::Top,
        AppPaneSide::Bottom => PaneSide::Bottom,
        AppPaneSide::Left => PaneSide::Left,
        AppPaneSide::Right => PaneSide::Right,
    }
}
