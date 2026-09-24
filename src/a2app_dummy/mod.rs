//! Dummy a2app widgets for builds without the `a2app` feature.
//!
//! DSL references to these widget names must always resolve at runtime,
//! so non-a2app builds register invisible stubs under the same names.

use makepad_widgets::*;

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    mod.widgets.MiniAppsScreen = View { visible: false }
    mod.widgets.RoomAppPicker = View { visible: false }
    mod.widgets.MiniAppHostPane = View { visible: false }
    mod.widgets.MiniAppPermissionPrompt = View { visible: false }
    mod.widgets.AiRoomPanel = View { visible: false }
    mod.widgets.MiniAppTimelineCard = View { visible: false }
    mod.widgets.AiReplyTimelineCard = View { visible: false }
    mod.widgets.AiEventTimelineCard = View { visible: false }
    mod.widgets.AiTurnTimelineCard = View { visible: false }
    mod.widgets.AiTurnSpinner = View { visible: false }
    mod.widgets.MiniAppHostArea = View { visible: false }
}

/// Stubs for the mini-app room panes, which only exist with the `a2app` feature.
pub mod room_panes {
    use makepad_widgets::*;
    use matrix_sdk::ruma::RoomId;

    use crate::room::room_pane::PaneSide;

    pub fn app_name(_app_id: &str) -> Option<String> { None }
    pub fn app_glyph(_app_id: &str) -> Option<String> { None }
    pub fn attach(
        _cx: &mut Cx,
        _host_area: &WidgetRef,
        _surface_uid: WidgetUid,
        _room_id: &RoomId,
        _app_id: &str,
        _side: Option<PaneSide>,
        _is_spawn_allowed: bool,
    ) -> bool { false }
    pub fn detach(_cx: &mut Cx, _host_area: &WidgetRef, _surface_uid: WidgetUid, _room_id: &RoomId, _app_id: &str, _is_quitting: bool) {}
    pub fn is_shown_by(_room_id: &RoomId, _app_id: &str, _surface_uid: WidgetUid) -> bool { false }
    pub fn is_running(_room_id: &RoomId, _app_id: &str) -> bool { false }
    pub fn quit_if_parked(_cx: &mut Cx, _room_id: &RoomId, _app_id: &str) {}
    pub fn parked_apps(_room_id: &RoomId) -> Vec<String> { Vec::new() }
    pub fn note_side(_room_id: &RoomId, _app_id: &str, _side: PaneSide) {}
    pub fn note_size(_host_area: &WidgetRef, _room_id: &RoomId, _app_id: &str) {}
    pub fn release_all_shown_by(_surface_uid: WidgetUid) {}
}
