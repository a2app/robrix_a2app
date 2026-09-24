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
use super::permission_choices::*;

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    let GroupRow = View {
        width: Fill, height: Fit, spacing: 8
        flow: Right, align: Align{y: 0.5}
    }
    let ChangeButton = RobrixNeutralIconButton {
        text: "Change…"
        icon_walk: Walk{width: 0, height: 0, margin: 0}
    }

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
                ai_power_toggle := RobrixSettingsToggle {
                    active: true
                    text: "Allow this room's AI to run"
                }
            }
            permission_overview := View {
                width: Fill, height: Fit, flow: Down, spacing: 12
                SubsectionLabel { text: "Permissions", margin: 0 }
                ModalBody { text: "See what this agent can do. Choose Change to review one permission." }
                read_row := GroupRow {
                    read_state := ModalBody {}
                    read_change := ChangeButton {}
                }
                info_row := GroupRow {
                    info_state := ModalBody {}
                    info_change := ChangeButton {}
                }
                gen_row := GroupRow {
                    gen_state := ModalBody {}
                    gen_change := ChangeButton {}
                }
                app_use_row := GroupRow {
                    app_use_state := ModalBody {}
                    app_use_change := ChangeButton {}
                }
                rooms_read_row := GroupRow {
                    rooms_read_state := ModalBody {}
                    rooms_read_change := ChangeButton {}
                }
                rooms_list_row := GroupRow {
                    rooms_list_state := ModalBody {}
                    rooms_list_change := ChangeButton {}
                }
                spaces_row := GroupRow {
                    spaces_state := ModalBody {}
                    spaces_change := ChangeButton {}
                }
                rooms_send_row := GroupRow {
                    rooms_send_state := ModalBody {}
                    rooms_send_change := ChangeButton {}
                }
                tools_row := GroupRow {
                    tools_state := ModalBody {}
                    tools_change := ChangeButton {}
                }
                net_row := GroupRow {
                    net_state := ModalBody {}
                    net_change := ChangeButton {}
                }
            }
            permission_editor := View {
                visible: false
                width: Fill, height: Fit, flow: Down, spacing: 12
                back_permissions := RobrixNeutralIconButton {
                    text: "Back to permissions"
                    icon_walk: Walk{width: 0, height: 0, margin: 0}
                }
                editor_title := SettingsItemLabel { width: Fill, flow: Flow.Right{wrap: true} }
                editor_current := ModalBody {}
                editor_explanation := ModalBody {}
                editor_network_warning := ModalBody {
                    visible: false
                    text: "Internet access can let this agent send room messages, files or other local data off this device to online services. What is shared depends on what it does."
                }
                editor_choice := PermissionChoices {}
                ModalBody {
                    text: "Choose an option to apply it now. Ask when needed keeps existing approvals. Room and space protections still take priority."
                }
                abilities_button := RobrixNeutralIconButton {
                    text: "What this permission covers…"
                    icon_walk: Walk{width: 0, height: 0, margin: 0}
                }
                abilities_section := View {
                    visible: false
                    width: Fill, height: Fit, flow: Down, spacing: 8
                    ability_details := ModalBody {}
                    ModalBody { text: "Individual ability settings and scoped approvals are available in Mini Apps → Agent permissions." }
                }
            }
            extras_row := View {
                width: Fill, height: Fit, flow: Down, spacing: 8
                extras_state := ModalBody {}
                forget_extras_button := RobrixNegativeIconButton {
                    text: "Remove these approvals"
                    icon_walk: Walk{width: 0, height: 0, margin: 0}
                }
            }
            usage_label := ModalBody {}
            restrict_row := View {
                width: Fill, height: Fit, flow: Down, spacing: 8
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
    #[rust] rows: Vec<String>,
    #[rust] selected_group: Option<usize>,
    #[rust] abilities_expanded: bool,
    #[rust] has_extras: bool,
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
        if self.view.button(cx, ids!(back_permissions)).clicked(actions) {
            self.selected_group = None;
            self.abilities_expanded = false;
            self.update_editor(cx);
            self.view.view(cx, ids!(panel_content)).set_scroll_pos(cx, Vec2d::default());
            return;
        }
        if self.selected_group.is_none() {
            for (index, (_, _, _, change)) in panel_groups().into_iter().enumerate() {
                if self.view.button(cx, &[change]).clicked(actions) {
                    self.open_group(cx, index);
                    return;
                }
            }
        }
        if let Some(index) = self.selected_group {
            if self.view.button(cx, ids!(abilities_button)).clicked(actions) {
                self.abilities_expanded = !self.abilities_expanded;
                self.update_abilities(cx);
            }
            if let Some(choice) = self.view.permission_choices(cx, ids!(editor_choice)).changed(actions)
                && let Some((perm, _, _, _)) = panel_groups().get(index).copied()
            {
                let command = match (scoped_only(perm), choice) {
                    (true, 0) | (false, 1) => Some(AiRoomPanelCommand::Ask),
                    (true, 1) | (false, 2) => Some(AiRoomPanelCommand::Deny),
                    (false, 0) => Some(AiRoomPanelCommand::Allow),
                    _ => None,
                };
                if let Some(command) = command {
                    emit_panel_action(cx, &room_id, Some(perm), command);
                }
            }
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

fn panel_groups() -> [(Permission, LiveId, LiveId, LiveId); 10] {
    [
        (Permission::MatrixRoomRead, id!(read_row), id!(read_state), id!(read_change)),
        (Permission::MatrixRoomInfo, id!(info_row), id!(info_state), id!(info_change)),
        (Permission::AppGeneration, id!(gen_row), id!(gen_state), id!(gen_change)),
        (Permission::AppLaunch, id!(app_use_row), id!(app_use_state), id!(app_use_change)),
        (Permission::MatrixRoomsRead, id!(rooms_read_row), id!(rooms_read_state), id!(rooms_read_change)),
        (Permission::MatrixRoomsList, id!(rooms_list_row), id!(rooms_list_state), id!(rooms_list_change)),
        (Permission::MatrixSpaces, id!(spaces_row), id!(spaces_state), id!(spaces_change)),
        (Permission::MatrixRoomsSend, id!(rooms_send_row), id!(rooms_send_state), id!(rooms_send_change)),
        (Permission::McpTools, id!(tools_row), id!(tools_state), id!(tools_change)),
        (Permission::Network, id!(net_row), id!(net_state), id!(net_change)),
    ]
}

fn scoped_only(perm: Permission) -> bool {
    matches!(perm, Permission::MatrixRoomsSend | Permission::McpTools | Permission::Network)
}

impl AiRoomPanel {
    fn open_group(&mut self, cx: &mut Cx, index: usize) {
        if self.rows.get(index).is_none_or(String::is_empty) { return }
        self.selected_group = Some(index);
        self.abilities_expanded = false;
        self.update_editor(cx);
        self.view.view(cx, ids!(panel_content)).set_scroll_pos(cx, Vec2d::default());
    }

    fn update_abilities(&mut self, cx: &mut Cx) {
        self.view.widget(cx, ids!(abilities_section)).set_visible(cx, self.abilities_expanded);
        self.view.button(cx, ids!(abilities_button)).set_text(cx, if self.abilities_expanded {
            "Hide included abilities"
        } else { "What this permission covers…" });
        self.view.redraw(cx);
    }

    fn update_editor(&mut self, cx: &mut Cx) {
        if self.selected_group.is_some_and(|index| self.rows.get(index).is_none_or(String::is_empty)) {
            self.selected_group = None;
        }
        let editing = self.selected_group.is_some();
        self.view.widget(cx, ids!(permission_overview)).set_visible(cx, !editing);
        self.view.widget(cx, ids!(permission_editor)).set_visible(cx, editing);
        self.view.widget(cx, ids!(power_row)).set_visible(cx, !editing);
        self.view.widget(cx, ids!(extras_row)).set_visible(cx, !editing && self.has_extras);
        self.view.widget(cx, ids!(usage_label)).set_visible(cx, !editing);
        if let Some(index) = self.selected_group {
            let (perm, _, _, _) = panel_groups()[index];
            self.view.label(cx, ids!(editor_title)).set_text(cx, match perm {
                Permission::McpTools => "Use mini-app tools",
                Permission::Network => "Internet access",
                other => other.title(),
            });
            self.view.label(cx, ids!(editor_current)).set_text(cx, &self.rows[index]);
            let explanation = if scoped_only(perm) {
                "This permission is approved separately for each destination or tool. Ask when needed permits those approvals; Block stops all use of this permission for this agent."
            } else {
                "Always allow lasts until you change it. Block stops this permission for this agent, including existing approvals."
            };
            self.view.label(cx, ids!(editor_explanation)).set_text(cx, explanation);
            self.view.widget(cx, ids!(editor_network_warning)).set_visible(cx, perm == Permission::Network);
            let labels = if scoped_only(perm) { vec!["Ask when needed".into(), "Block".into()] }
            else { vec!["Always allow".into(), "Ask when needed".into(), "Block".into()] };
            let choices = self.view.permission_choices(cx, ids!(editor_choice));
            choices.set_labels(cx, labels);
            // These are explicit actions, not a guessed saved setting. The effective state
            // above can be blocked by room protections even when this group is allowed.
            choices.set_selected_item(cx, usize::MAX);
            let details = super::ai::tools::AI_ROOM_SESSION_CAP_IDS.iter()
                .filter_map(|id| a2app_core::capabilities::by_id(id))
                .filter(|cap| cap.group == Some(perm))
                .map(|cap| format!("• {}: {}", cap.title, cap.blurb))
                .collect::<Vec<_>>().join("\n\n");
            self.view.label(cx, ids!(ability_details)).set_text(cx, &details);
        }
        self.update_abilities(cx);
    }

    fn populate(&mut self, cx: &mut Cx, info: &AiRoomPanelInfo) {
        let different_room = self.room_id.as_ref() != Some(&info.room_id);
        self.room_id = Some(info.room_id.clone());
        self.rows.clone_from(&info.rows);
        if different_room {
            self.selected_group = None;
            self.abilities_expanded = false;
            self.view.view(cx, ids!(panel_content)).set_scroll_pos(cx, Vec2d::default());
        }
        self.view.label(cx, ids!(panel_title)).set_text(cx, &format!("AI in {}", info.room_name));
        self.view.check_box(cx, ids!(ai_power_toggle)).set_active(cx, info.powered_on, Animate::No);
        for (index, (_, row, state, _)) in panel_groups().into_iter().enumerate() {
            let text = info.rows.get(index).map(String::as_str).unwrap_or("");
            self.view.label(cx, &[state]).set_text(cx, text);
            self.view.widget(cx, &[row]).set_visible(cx, !text.is_empty());
        }
        let extras = info.extras.as_deref().unwrap_or_default();
        self.has_extras = !extras.is_empty();
        self.view.label(cx, ids!(extras_state)).set_text(cx, extras);
        self.view.label(cx, ids!(usage_label)).set_text(cx, &info.usage);
        let notice = info.restriction.as_deref().unwrap_or_default();
        let restricted = !notice.is_empty();
        self.view.label(cx, ids!(restrict_notice)).set_text(cx, notice);
        self.view.widget(cx, ids!(restrict_row)).set_visible(cx, restricted);
        self.view.button(cx, ids!(unrestrict_button)).set_visible(cx, restricted);
        self.update_editor(cx);
    }

}

impl AiRoomPanelRef {
    /// Populates the panel for one AI room and makes it visible within the
    /// (already-open) modal. Call again after every action to refresh state.
    pub fn show(&self, cx: &mut Cx, info: &AiRoomPanelInfo) {
        if let Some(mut inner) = self.borrow_mut() { inner.populate(cx, info); }
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
            super::super::permission_choices::script_mod(vm);
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

    fn click(panel: &AiRoomPanelRef, cx: &mut Cx, path: &[LiveId]) -> Vec<AiRoomPanelAction> {
        let uid = panel.borrow().unwrap().view.button(cx, path).widget_uid();
        let click = cx.capture_actions(|cx| cx.widget_action(uid, ButtonAction::Clicked(Default::default())));
        let actions = cx.capture_actions(|cx| panel.borrow_mut().unwrap().handle_event(cx, &Event::Actions(click), &mut Scope::empty()));
        actions.iter().filter_map(|action| action.downcast_ref::<AiRoomPanelAction>()).cloned().collect()
    }

    fn choose(panel: &AiRoomPanelRef, cx: &mut Cx, index: usize) -> Vec<AiRoomPanelAction> {
        let uid = panel.borrow().unwrap().view.permission_choices(cx, ids!(editor_choice)).widget_uid();
        let changed = cx.capture_actions(|cx| cx.widget_action(uid, PermissionChoicesAction::Changed(index)));
        let actions = cx.capture_actions(|cx| panel.borrow_mut().unwrap().handle_event(cx, &Event::Actions(changed), &mut Scope::empty()));
        actions.iter().filter_map(|action| action.downcast_ref::<AiRoomPanelAction>()).cloned().collect()
    }

    #[test]
    fn opening_permissions_and_ability_details_never_changes_access() {
        let (mut cx, panel) = panel();
        let mut info = info();
        info.rows[0] = "Read this room — don't allow".into();
        panel.show(&mut cx, &info);
        assert!(click(&panel, &mut cx, ids!(read_change)).is_empty());
        assert!(!panel.borrow().unwrap().view.widget(&cx, ids!(permission_overview)).visible());
        assert!(panel.borrow().unwrap().view.widget(&cx, ids!(permission_editor)).visible());
        assert_eq!(panel.borrow().unwrap().view.label(&cx, ids!(editor_current)).text(), info.rows[0]);
        assert_eq!(panel.borrow().unwrap().view.permission_choices(&cx, ids!(editor_choice)).selected_item(), usize::MAX,
            "a blocked effective state must not be mistaken for a saved group denial");
        assert!(click(&panel, &mut cx, ids!(abilities_button)).is_empty());
        assert!(panel.borrow().unwrap().view.widget(&cx, ids!(abilities_section)).visible());
        assert!(!panel.borrow().unwrap().view.label(&cx, ids!(ability_details)).text().is_empty());
        let actions = choose(&panel, &mut cx, 0);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].command, AiRoomPanelCommand::Allow);
        assert_eq!(actions[0].perm, Some(Permission::MatrixRoomRead));
        assert!(click(&panel, &mut cx, ids!(back_permissions)).is_empty());
        assert!(panel.borrow().unwrap().view.widget(&cx, ids!(permission_overview)).visible());
        assert!(choose(&panel, &mut cx, 0).is_empty(), "a hidden editor cannot apply a stale choice");
    }

    #[test]
    fn scoped_groups_never_offer_blanket_allow_and_editor_resets_for_another_room() {
        let (mut cx, panel) = panel();
        let mut info = info();
        info.rows.resize(10, String::new());
        info.rows[9] = "Internet access — ask when needed".into();
        panel.show(&mut cx, &info);
        assert!(click(&panel, &mut cx, ids!(net_change)).is_empty());
        assert!(choose(&panel, &mut cx, 2).is_empty(), "unknown choices cannot become blanket internet access");
        for (index, command) in [(0, AiRoomPanelCommand::Ask), (1, AiRoomPanelCommand::Deny)] {
            let actions = choose(&panel, &mut cx, index);
            assert_eq!(actions.len(), 1);
            assert_eq!(actions[0].command, command);
            assert_eq!(actions[0].perm, Some(Permission::Network));
        }
        info.room_id = "!another:example.org".into();
        panel.show(&mut cx, &info);
        assert!(panel.borrow().unwrap().view.widget(&cx, ids!(permission_overview)).visible());
        assert!(panel.borrow().unwrap().selected_group.is_none());
        assert!(choose(&panel, &mut cx, 0).is_empty());
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
