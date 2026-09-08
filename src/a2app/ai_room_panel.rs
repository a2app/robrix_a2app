//! The "AI in this room" management panel: what this room's agent may do,
//! its session power, its recent use, and any restriction it earned.
//!
//! One panel per AI room, opened from the room's input bar (`/ai`). It is the
//! room-scoped equivalent of a mini-app's App Info: the same [`PermissionStore`]
//! rows (group answers under the room's agent subject), the same access
//! records, and the same restriction clear — so a *Deny* given at a permission
//! prompt is always recoverable here, and a session the host stopped for
//! flooding can be let back in.
//!
//! The widget is deliberately dumb: it shows strings the runtime computed and
//! emits [`AiRoomPanelAction`]s; the runtime applies them and re-shows the
//! panel with fresh state.

use makepad_widgets::*;
use a2app_core::permissions::Permission;

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    mod.widgets.AiRoomPanel = set_type_default() do #(AiRoomPanel::register_widget(vm)) {
        ..mod.widgets.SmallModal

        width: Fill { max: 560 }
        flow: Down
        spacing: 10
        padding: Inset{top: 16, bottom: 16, left: 18, right: 18}

        panel_title := ModalTitle {
            text: "AI in this room"
        }
        panel_subtitle := ModalBody {
            body: "<p>What this room's AI may do, and whether it's running. Changes apply to your device only.</p>"
        }

        power_row := View {
            width: Fill, height: Fit
            flow: Down
            spacing: 2
            ai_power_toggle := ToggleFlat {
                active: true
                draw_bg +: { size: 21 }
                text: "This room's AI is on"
                draw_text +: { text_style: theme.font_bold {font_size: 11} }
            }
        }

        read_row := View {
            width: Fill, height: Fit
            flow: Down
            spacing: 4
            read_state := ModalBody {}
            read_buttons := View {
                width: Fill, height: Fit
                flow: Right
                spacing: 8
                read_allow := ButtonFlat { text: "Allow" }
                read_ask := ButtonFlat { text: "Ask each time" }
                read_block := ButtonFlatter { text: "Don't allow" }
            }
        }

        info_row := View {
            width: Fill, height: Fit
            flow: Down
            spacing: 4
            info_state := ModalBody {}
            info_buttons := View {
                width: Fill, height: Fit
                flow: Right
                spacing: 8
                info_allow := ButtonFlat { text: "Allow" }
                info_ask := ButtonFlat { text: "Ask each time" }
                info_block := ButtonFlatter { text: "Don't allow" }
            }
        }

        gen_row := View {
            width: Fill, height: Fit
            flow: Down
            spacing: 4
            gen_state := ModalBody {}
            gen_buttons := View {
                width: Fill, height: Fit
                flow: Right
                spacing: 8
                gen_allow := ButtonFlat { text: "Allow" }
                gen_ask := ButtonFlat { text: "Ask each time" }
                gen_block := ButtonFlatter { text: "Don't allow" }
            }
        }

        usage_label := ModalBody {}

        restrict_row := View {
            width: Fill, height: Fit
            flow: Down
            spacing: 4
            restrict_notice := ModalBody {}
            unrestrict_button := ButtonFlat { text: "Let it run again", visible: false }
        }
    }
}

/// What one panel row's button asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AiRoomPanelCommand {
    /// Store `Granted` (no prompt until revoked).
    Allow,
    /// Store nothing (`Ask`): prompt on next use.
    Ask,
    /// Store `Denied`.
    Deny,
    /// Let a restricted session run again.
    Unrestrict,
    /// Turn the session power on.
    PowerOn,
    /// Turn the session power off (stops the agent).
    PowerOff,
}

/// A user action from the panel, applied by the a2app runtime.
#[derive(Clone, Debug)]
pub struct AiRoomPanelAction {
    pub room_id: String,
    pub perm: Option<Permission>,
    pub command: AiRoomPanelCommand,
}

/// What the panel needs to render one state.
pub struct AiRoomPanelInfo {
    pub room_id: String,
    pub room_name: String,
    /// Whether the session is running/turned on.
    pub powered_on: bool,
    /// One formatted line per managed group, e.g.
    /// `"Read room content — allowed · 3 uses · last 2m ago"`.
    pub rows: Vec<String>,
    pub usage: String,
    pub restriction: Option<String>,
}

#[derive(Script, ScriptHook, Widget)]
pub struct AiRoomPanel {
    #[deref] view: View,
    /// The room this panel manages, set at [`AiRoomPanelRef::show`] time.
    #[rust] room_id: Option<String>,
}

/// Emits one panel action (a helper so `handle_event` never holds a closure
/// over `cx` while also querying widgets).
fn emit_panel_action(
    cx: &mut Cx,
    room_id: &str,
    perm: Option<Permission>,
    command: AiRoomPanelCommand,
) {
    cx.action(AiRoomPanelAction { room_id: room_id.to_string(), perm, command });
}

impl Widget for AiRoomPanel {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        let Event::Actions(actions) = event else { return };
        let Some(room_id) = self.room_id.clone() else { return };
        if let Some(on) = self.view.check_box(cx, ids!(ai_power_toggle)).changed(actions) {
            emit_panel_action(
                cx,
                &room_id,
                None,
                if on { AiRoomPanelCommand::PowerOn } else { AiRoomPanelCommand::PowerOff },
            );
            return;
        }
        // (perm, allow, ask, deny) button triples, one per managed group.
        if self.view.button(cx, ids!(read_allow)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixRoomRead), AiRoomPanelCommand::Allow);
        } else if self.view.button(cx, ids!(read_ask)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixRoomRead), AiRoomPanelCommand::Ask);
        } else if self.view.button(cx, ids!(read_block)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixRoomRead), AiRoomPanelCommand::Deny);
        } else if self.view.button(cx, ids!(info_allow)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixRoomInfo), AiRoomPanelCommand::Allow);
        } else if self.view.button(cx, ids!(info_ask)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixRoomInfo), AiRoomPanelCommand::Ask);
        } else if self.view.button(cx, ids!(info_block)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixRoomInfo), AiRoomPanelCommand::Deny);
        } else if self.view.button(cx, ids!(gen_allow)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::AppGeneration), AiRoomPanelCommand::Allow);
        } else if self.view.button(cx, ids!(gen_ask)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::AppGeneration), AiRoomPanelCommand::Ask);
        } else if self.view.button(cx, ids!(gen_block)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::AppGeneration), AiRoomPanelCommand::Deny);
        } else if self.view.button(cx, ids!(unrestrict_button)).clicked(actions) {
            emit_panel_action(cx, &room_id, None, AiRoomPanelCommand::Unrestrict);
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)
    }
}

impl AiRoomPanelRef {
    /// Populates the panel for one AI room and makes it visible within the
    /// (already-open) modal. Call again after every action to refresh state.
    pub fn show(&self, cx: &mut Cx, info: &AiRoomPanelInfo) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.room_id = Some(info.room_id.clone());
        let v = &mut inner.view;
        v.label(cx, ids!(panel_title)).set_text(cx, &format!("AI in {}", info.room_name));
        v.check_box(cx, ids!(ai_power_toggle))
            .set_active(cx, info.powered_on, Animate::No);
        let row = |i: usize| info.rows.get(i).map(String::as_str).unwrap_or("");
        v.label(cx, ids!(read_state)).set_text(cx, row(0));
        v.label(cx, ids!(info_state)).set_text(cx, row(1));
        v.label(cx, ids!(gen_state)).set_text(cx, row(2));
        v.label(cx, ids!(usage_label)).set_text(cx, &info.usage);
        let restrict_notice = info.restriction.clone().unwrap_or_default();
        let restricted = !restrict_notice.is_empty();
        v.label(cx, ids!(restrict_notice)).set_text(cx, &restrict_notice);
        v.label(cx, ids!(restrict_notice)).set_visible(cx, restricted);
        v.button(cx, ids!(unrestrict_button)).set_visible(cx, restricted);
        v.redraw(cx);
    }
}
