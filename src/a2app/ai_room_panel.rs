//! The "AI in this room" management panel: what this room's agent may do,
//! whether it can run, its recent use, and any temporary restriction.
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

        width: Fill { max: 640 }
        height: Fill { max: 760 }
        padding: 20
        scroll_bars: ScrollBars { show_scroll_x: false, show_scroll_y: false }

        panel_title := ModalTitle {
            margin: Inset{bottom: 14}
            text: "AI in this room"
        }
        panel_content := ScrollYView {
            width: Fill, height: Fill, flow: Down, spacing: 16
            padding: Inset{right: 8}
            panel_subtitle := ModalBody {
                text: "Room protections and data-sharing rules also apply to this agent."
            }

            power_row := View {
                width: Fill, height: Fit
                flow: Down
                spacing: 2
                ai_power_toggle := RobrixSettingsToggle {
                    active: true
                    text: "Allow this room's AI to run"
                }
            }
            SubsectionLabel { text: "Permissions", margin: 0 }
            ModalBody {
                text: "Always allow lasts until changed. Ask when needed keeps saved approvals. Block stops this permission everywhere for this agent."
            }

            read_row := View {
                width: Fill, height: Fit
                flow: Down
                spacing: 4
                read_state := ModalBody {}
                read_buttons := View {
                    width: Fill, height: Fit
                    flow: Flow.Right{wrap: true}
                    spacing: 8
                    read_allow := RobrixPositiveIconButton { text: "Always allow", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                    read_ask := RobrixNeutralIconButton { text: "Ask when needed", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                    read_block := RobrixNegativeIconButton { text: "Block", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                }
            }

            info_row := View {
                width: Fill, height: Fit
                flow: Down
                spacing: 4
                info_state := ModalBody {}
                info_buttons := View {
                    width: Fill, height: Fit
                    flow: Flow.Right{wrap: true}
                    spacing: 8
                    info_allow := RobrixPositiveIconButton { text: "Always allow", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                    info_ask := RobrixNeutralIconButton { text: "Ask when needed", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                    info_block := RobrixNegativeIconButton { text: "Block", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                }
            }

            gen_row := View {
                width: Fill, height: Fit
                flow: Down
                spacing: 4
                gen_state := ModalBody {}
                gen_buttons := View {
                    width: Fill, height: Fit
                    flow: Flow.Right{wrap: true}
                    spacing: 8
                    gen_allow := RobrixPositiveIconButton { text: "Always allow", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                    gen_ask := RobrixNeutralIconButton { text: "Ask when needed", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                    gen_block := RobrixNegativeIconButton { text: "Block", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                }
            }

            app_use_row := View {
                width: Fill, height: Fit
                flow: Down
                spacing: 4
                app_use_state := ModalBody {}
                app_use_buttons := View {
                    width: Fill, height: Fit
                    flow: Flow.Right{wrap: true}
                    spacing: 8
                    app_use_allow := RobrixPositiveIconButton { text: "Always allow", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                    app_use_ask := RobrixNeutralIconButton { text: "Ask when needed", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                    app_use_block := RobrixNegativeIconButton { text: "Block", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                }
            }

            rooms_read_row := View {
                width: Fill, height: Fit
                flow: Down
                spacing: 4
                rooms_read_state := ModalBody {}
                rooms_read_buttons := View {
                    width: Fill, height: Fit
                    flow: Flow.Right{wrap: true}
                    spacing: 8
                    rooms_read_allow := RobrixPositiveIconButton { text: "Always allow", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                    rooms_read_ask := RobrixNeutralIconButton { text: "Ask when needed", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                    rooms_read_block := RobrixNegativeIconButton { text: "Block", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                }
            }

            rooms_list_row := View {
                width: Fill, height: Fit
                flow: Down
                spacing: 4
                rooms_list_state := ModalBody {}
                rooms_list_buttons := View {
                    width: Fill, height: Fit
                    flow: Flow.Right{wrap: true}
                    spacing: 8
                    rooms_list_allow := RobrixPositiveIconButton { text: "Always allow", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                    rooms_list_ask := RobrixNeutralIconButton { text: "Ask when needed", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                    rooms_list_block := RobrixNegativeIconButton { text: "Block", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                }
            }

            spaces_row := View {
                width: Fill, height: Fit
                flow: Down
                spacing: 4
                spaces_state := ModalBody {}
                spaces_buttons := View {
                    width: Fill, height: Fit
                    flow: Flow.Right{wrap: true}
                    spacing: 8
                    spaces_allow := RobrixPositiveIconButton { text: "Always allow", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                    spaces_ask := RobrixNeutralIconButton { text: "Ask when needed", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                    spaces_block := RobrixNegativeIconButton { text: "Block", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                }
            }

            rooms_send_row := View {
                width: Fill, height: Fit
                flow: Down
                spacing: 4
                rooms_send_state := ModalBody {}
                rooms_send_buttons := View {
                    width: Fill, height: Fit
                    flow: Flow.Right{wrap: true}
                    spacing: 8
                    rooms_send_ask := RobrixNeutralIconButton { text: "Ask when needed", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                    rooms_send_block := RobrixNegativeIconButton { text: "Block", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                }
            }

            tools_row := View {
                width: Fill, height: Fit
                flow: Down
                spacing: 4
                tools_state := ModalBody {}
                tools_buttons := View {
                    width: Fill, height: Fit
                    flow: Flow.Right{wrap: true}
                    spacing: 8
                    tools_ask := RobrixNeutralIconButton { text: "Ask when needed", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                    tools_block := RobrixNegativeIconButton { text: "Block", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                }
            }

            net_row := View {
                width: Fill, height: Fit
                flow: Down
                spacing: 4
                net_state := ModalBody {}
                net_buttons := View {
                    width: Fill, height: Fit
                    flow: Flow.Right{wrap: true}
                    spacing: 8
                    net_ask := RobrixNeutralIconButton { text: "Ask when needed", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                    net_block := RobrixNegativeIconButton { text: "Block", icon_walk: Walk{width: 0, height: 0, margin: 0} }
                }
            }

            extras_row := View {
                width: Fill, height: Fit
                flow: Down
                spacing: 4
                extras_state := ModalBody {}
                forget_extras_button := RobrixNegativeIconButton {
                    text: "Remove these approvals"
                    icon_walk: Walk{width: 0, height: 0, margin: 0}
                }
            }

            usage_label := ModalBody {}

            restrict_row := View {
                width: Fill, height: Fit
                flow: Down
                spacing: 4
                restrict_notice := ModalBody {}
                unrestrict_button := RobrixPositiveIconButton {
                    text: "Let it run again", visible: false
                    icon_walk: Walk{width: 0, height: 0, margin: 0}
                }
            }
        }
        ModalButtonsRow {
            padding: Inset{top: 16, bottom: 0}
            close_button := RobrixNeutralIconButton {
                text: "Close", padding: 12
                icon_walk: Walk{width: 0, height: 0, margin: 0}
            }
        }
    }
}

/// What one panel row's button asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AiRoomPanelCommand {
    /// Close the panel without changing permissions.
    Close,
    /// Store `Granted` (no permission prompt until revoked).
    Allow,
    /// Store `Ask`: prompt when no existing scoped approval permits use.
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
        if self.view.button(cx, ids!(close_button)).clicked(actions) {
            emit_panel_action(cx, &room_id, None, AiRoomPanelCommand::Close);
            return;
        }
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
        // Post into other rooms. Scoped approval is the most this row can offer: the real
        // answer is given per room / per tool / per site.
        if self.view.button(cx, ids!(rooms_send_ask)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixRoomsSend), AiRoomPanelCommand::Ask);
        } else if self.view.button(cx, ids!(rooms_send_block)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::MatrixRoomsSend), AiRoomPanelCommand::Deny);
        }
        // Call tools a mini-app offers here. Scoped approval is the most this row can offer: the real
        // answer is given per room / per tool / per site.
        if self.view.button(cx, ids!(tools_ask)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::McpTools), AiRoomPanelCommand::Ask);
        } else if self.view.button(cx, ids!(tools_block)).clicked(actions) {
            emit_panel_action(cx, &room_id, Some(Permission::McpTools), AiRoomPanelCommand::Deny);
        }
        // Reach the internet. Scoped approval is the most this row can offer: the real
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
        let different_room = inner.room_id.as_ref() != Some(&info.room_id);
        inner.room_id = Some(info.room_id.clone());
        let v = &mut inner.view;
        if different_room {
            v.view(cx, ids!(panel_content)).set_scroll_pos(cx, Vec2d::default());
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn panel() -> (Cx, AiRoomPanelRef) {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let panel = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            crate::shared::script_mod(vm);
            super::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.AiRoomPanel {} });
            WidgetRef::script_from_value(vm, value).as_ai_room_panel()
        });
        (cx, panel)
    }

    fn info() -> AiRoomPanelInfo {
        AiRoomPanelInfo {
            room_id: "!test:example.org".into(), room_name: "Test room".into(), powered_on: false,
            rows: vec!["Read this room — ask when needed".into()], extras: None,
            usage: "No tool use recorded yet.".into(), restriction: None,
        }
    }

    #[test]
    fn panel_hides_unavailable_permissions_and_clears_old_restrictions() {
        let (mut cx, panel) = panel();
        let mut info = info();
        info.extras = Some("Allowed website: example.org".into());
        info.restriction = Some("Paused after too many requests.".into());
        panel.show(&mut cx, &info);
        assert!(panel.borrow().unwrap().view.widget(&cx, ids!(read_row)).visible());
        assert!(!panel.borrow().unwrap().view.widget(&cx, ids!(net_row)).visible());
        assert!(panel.borrow().unwrap().view.widget(&cx, ids!(extras_row)).visible());
        assert!(panel.borrow().unwrap().view.widget(&cx, ids!(unrestrict_button)).visible());
        info.extras = None;
        info.restriction = None;
        panel.show(&mut cx, &info);
        assert!(!panel.borrow().unwrap().view.widget(&cx, ids!(extras_row)).visible());
        assert!(!panel.borrow().unwrap().view.widget(&cx, ids!(unrestrict_button)).visible());
    }

    #[test]
    fn close_emits_no_permission_or_power_change() {
        let (mut cx, panel) = panel();
        panel.show(&mut cx, &info());
        let mut inner = panel.borrow_mut().unwrap();
        let close = inner.view.button(&cx, ids!(close_button)).widget_uid();
        let click = cx.capture_actions(|cx| cx.widget_action(close, ButtonAction::Clicked(Default::default())));
        let actions = cx.capture_actions(|cx| inner.handle_event(cx, &Event::Actions(click), &mut Scope::empty()));
        let changes = actions.iter().filter_map(|action| action.downcast_ref::<AiRoomPanelAction>()).collect::<Vec<_>>();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].room_id, "!test:example.org");
        assert_eq!(changes[0].command, AiRoomPanelCommand::Close);
        assert_eq!(changes[0].perm, None);
    }
}
