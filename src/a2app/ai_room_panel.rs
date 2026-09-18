//! The "AI in this room" management panel: what this room's agent may do,
//! its session power, its recent use, and any restriction it earned.
//!
//! One panel per AI room, opened from the room's input bar (`/ai`). It is the
//! room-scoped equivalent of a mini-app's App Info: the same [`PermissionStore`]
//! rows (group answers under the room's agent subject), the same access
//! records, and the same restriction clear — so a *Deny* given at a permission
//! prompt is always recoverable here, and a session the host stopped for
//! flooding can be let back in. Rows whose group only acts as a kill switch
//! (other rooms, mini-app tools, the internet — each really decided per room,
//! per tool, per site) offer no *Allow*: it would grant nothing.
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
            text: "What this room's AI may do, and whether it's running. Changes apply to your device only."
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

        app_use_row := View {
            width: Fill, height: Fit
            flow: Down
            spacing: 4
            app_use_state := ModalBody {}
            app_use_buttons := View {
                width: Fill, height: Fit
                flow: Right
                spacing: 8
                app_use_allow := ButtonFlat { text: "Allow" }
                app_use_ask := ButtonFlat { text: "Ask each time" }
                app_use_block := ButtonFlatter { text: "Don't allow" }
            }
        }

        rooms_read_row := View {
            width: Fill, height: Fit
            flow: Down
            spacing: 4
            rooms_read_state := ModalBody {}
            rooms_read_buttons := View {
                width: Fill, height: Fit
                flow: Right
                spacing: 8
                rooms_read_allow := ButtonFlat { text: "Allow" }
                rooms_read_ask := ButtonFlat { text: "Ask each time" }
                rooms_read_block := ButtonFlatter { text: "Don't allow" }
            }
        }

        rooms_list_row := View {
            width: Fill, height: Fit
            flow: Down
            spacing: 4
            rooms_list_state := ModalBody {}
            rooms_list_buttons := View {
                width: Fill, height: Fit
                flow: Right
                spacing: 8
                rooms_list_allow := ButtonFlat { text: "Allow" }
                rooms_list_ask := ButtonFlat { text: "Ask each time" }
                rooms_list_block := ButtonFlatter { text: "Don't allow" }
            }
        }

        spaces_row := View {
            width: Fill, height: Fit
            flow: Down
            spacing: 4
            spaces_state := ModalBody {}
            spaces_buttons := View {
                width: Fill, height: Fit
                flow: Right
                spacing: 8
                spaces_allow := ButtonFlat { text: "Allow" }
                spaces_ask := ButtonFlat { text: "Ask each time" }
                spaces_block := ButtonFlatter { text: "Don't allow" }
            }
        }

        rooms_send_row := View {
            width: Fill, height: Fit
            flow: Down
            spacing: 4
            rooms_send_state := ModalBody {}
            rooms_send_buttons := View {
                width: Fill, height: Fit
                flow: Right
                spacing: 8
                rooms_send_ask := ButtonFlat { text: "Ask each time" }
                rooms_send_block := ButtonFlatter { text: "Don't allow" }
            }
        }

        tools_row := View {
            width: Fill, height: Fit
            flow: Down
            spacing: 4
            tools_state := ModalBody {}
            tools_buttons := View {
                width: Fill, height: Fit
                flow: Right
                spacing: 8
                tools_ask := ButtonFlat { text: "Ask each time" }
                tools_block := ButtonFlatter { text: "Don't allow" }
            }
        }

        net_row := View {
            width: Fill, height: Fit
            flow: Down
            spacing: 4
            net_state := ModalBody {}
            net_buttons := View {
                width: Fill, height: Fit
                flow: Right
                spacing: 8
                net_ask := ButtonFlat { text: "Ask each time" }
                net_block := ButtonFlatter { text: "Don't allow" }
            }
        }

        extras_row := View {
            width: Fill, height: Fit
            flow: Down
            spacing: 4
            extras_state := ModalBody {}
            forget_extras_button := ButtonFlatter { text: "Forget these" }
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
    /// Forget the individual rooms, sites and tools this AI was allowed.
    ForgetExtras,
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
    /// The one-at-a-time grants (rooms, sites, mini-app tools) this AI holds,
    /// as one line; `None` when it holds none.
    pub extras: Option<String>,
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
        // Read this room's content.
        if self.view.button(cx, ids!(read_allow)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixRoomRead), AiRoomPanelCommand::Allow);
        } else if self.view.button(cx, ids!(read_ask)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixRoomRead), AiRoomPanelCommand::Ask);
        } else if self.view.button(cx, ids!(read_block)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixRoomRead), AiRoomPanelCommand::Deny);
        }
        // See this room's details.
        if self.view.button(cx, ids!(info_allow)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixRoomInfo), AiRoomPanelCommand::Allow);
        } else if self.view.button(cx, ids!(info_ask)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixRoomInfo), AiRoomPanelCommand::Ask);
        } else if self.view.button(cx, ids!(info_block)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixRoomInfo), AiRoomPanelCommand::Deny);
        }
        // Build and run mini-apps.
        if self.view.button(cx, ids!(gen_allow)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::AppGeneration), AiRoomPanelCommand::Allow);
        } else if self.view.button(cx, ids!(gen_ask)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::AppGeneration), AiRoomPanelCommand::Ask);
        } else if self.view.button(cx, ids!(gen_block)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::AppGeneration), AiRoomPanelCommand::Deny);
        }
        // Open your mini-apps.
        if self.view.button(cx, ids!(app_use_allow)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::AppLaunch), AiRoomPanelCommand::Allow);
        } else if self.view.button(cx, ids!(app_use_ask)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::AppLaunch), AiRoomPanelCommand::Ask);
        } else if self.view.button(cx, ids!(app_use_block)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::AppLaunch), AiRoomPanelCommand::Deny);
        }
        // Read messages in other rooms.
        if self.view.button(cx, ids!(rooms_read_allow)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixRoomsRead), AiRoomPanelCommand::Allow);
        } else if self.view.button(cx, ids!(rooms_read_ask)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixRoomsRead), AiRoomPanelCommand::Ask);
        } else if self.view.button(cx, ids!(rooms_read_block)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixRoomsRead), AiRoomPanelCommand::Deny);
        }
        // See a list of the user's rooms.
        if self.view.button(cx, ids!(rooms_list_allow)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixRoomsList), AiRoomPanelCommand::Allow);
        } else if self.view.button(cx, ids!(rooms_list_ask)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixRoomsList), AiRoomPanelCommand::Ask);
        } else if self.view.button(cx, ids!(rooms_list_block)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixRoomsList), AiRoomPanelCommand::Deny);
        }
        // Your spaces.
        if self.view.button(cx, ids!(spaces_allow)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixSpaces), AiRoomPanelCommand::Allow);
        } else if self.view.button(cx, ids!(spaces_ask)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixSpaces), AiRoomPanelCommand::Ask);
        } else if self.view.button(cx, ids!(spaces_block)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixSpaces), AiRoomPanelCommand::Deny);
        }
        // Post into other rooms. Asking each time is the most this row can offer: the real
        // answer is given per room / per tool / per site.
        if self.view.button(cx, ids!(rooms_send_ask)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixRoomsSend), AiRoomPanelCommand::Ask);
        } else if self.view.button(cx, ids!(rooms_send_block)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixRoomsSend), AiRoomPanelCommand::Deny);
        }
        // Call tools a mini-app offers here. Asking each time is the most this row can offer: the real
        // answer is given per room / per tool / per site.
        if self.view.button(cx, ids!(tools_ask)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::McpTools), AiRoomPanelCommand::Ask);
        } else if self.view.button(cx, ids!(tools_block)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::McpTools), AiRoomPanelCommand::Deny);
        }
        // Reach the internet. Asking each time is the most this row can offer: the real
        // answer is given per room / per tool / per site.
        if self.view.button(cx, ids!(net_ask)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::Network), AiRoomPanelCommand::Ask);
        } else if self.view.button(cx, ids!(net_block)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::Network), AiRoomPanelCommand::Deny);
        }
        if self.view.button(cx, ids!(forget_extras_button)).clicked(actions) {
            emit_panel_action(cx, &room_id, None, AiRoomPanelCommand::ForgetExtras);
        }
        if self.view.button(cx, ids!(unrestrict_button)).clicked(actions) {
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
        v.label(cx, ids!(app_use_state)).set_text(cx, row(3));
        v.label(cx, ids!(rooms_read_state)).set_text(cx, row(4));
        v.label(cx, ids!(rooms_list_state)).set_text(cx, row(5));
        v.label(cx, ids!(spaces_state)).set_text(cx, row(6));
        v.label(cx, ids!(rooms_send_state)).set_text(cx, row(7));
        v.label(cx, ids!(tools_state)).set_text(cx, row(8));
        v.label(cx, ids!(net_state)).set_text(cx, row(9));
        // A group this AI does not offer has no line, so its row is hidden.
        for (i, id) in [
            ids!(read_row), ids!(info_row), ids!(gen_row), ids!(app_use_row),
            ids!(rooms_read_row), ids!(rooms_list_row), ids!(spaces_row),
            ids!(rooms_send_row), ids!(tools_row), ids!(net_row),
        ]
        .into_iter()
        .enumerate()
        {
            v.widget(cx, id).set_visible(cx, !row(i).is_empty());
        }
        let extras = info.extras.clone().unwrap_or_default();
        v.label(cx, ids!(extras_state)).set_text(cx, &extras);
        v.widget(cx, ids!(extras_row)).set_visible(cx, !extras.is_empty());
        v.label(cx, ids!(usage_label)).set_text(cx, &info.usage);
        let restrict_notice = info.restriction.clone().unwrap_or_default();
        let restricted = !restrict_notice.is_empty();
        v.label(cx, ids!(restrict_notice)).set_text(cx, &restrict_notice);
        v.label(cx, ids!(restrict_notice)).set_visible(cx, restricted);
        v.button(cx, ids!(unrestrict_button)).set_visible(cx, restricted);
        v.redraw(cx);
    }
}
