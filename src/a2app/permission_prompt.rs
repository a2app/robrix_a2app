//! The runtime permission prompt for mini-apps: "«App» wants to «do X»",
//! with explicit scope and duration, Allow Once, Block Everywhere, and Not Now.
//!
//! Shown one at a time; the queue lives in [`crate::a2app::runtime`].

use makepad_widgets::*;
use std::collections::BTreeSet;
use a2app_core::permissions::{GrantDuration, NetworkScope, NetworkScopeKind, Permission, RoomScope};
use crate::home::rooms_list::RoomsListRef;
use crate::shared::popup_list::{enqueue_popup_notification, PopupKind};
use super::permission_choices::*;

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    mod.widgets.PermissionOptionLabel = Label {
        width: Fill, height: Fit, padding: 0, margin: 0
        flow: Flow.Right{wrap: true}
        draw_text +: { text_style: SETTINGS_REGULAR_TEXT_STYLE {}, color: (MESSAGE_TEXT_COLOR) }
    }
    mod.widgets.PermissionDropDown = RobrixSettingsDropDown {
        width: Fill, margin: 0
        draw_text +: { max_lines: 1, text_overflow: TextOverflow.Ellipsis }
    }
    mod.widgets.PermissionScopeEditor = set_type_default() do #(PermissionScopeEditor::register_widget(vm)) {
        width: Fill, height: Fit, flow: Down, spacing: 8
        SettingsItemLabel { text: "Rooms and spaces" }
        target_summary := mod.widgets.PermissionOptionLabel {}
        choose_targets := RobrixNeutralIconButton {
            text: "Change rooms…"
            icon_walk: Walk{width: 0, height: 0, margin: 0}
        }
        selected_targets := View {
            visible: false
            width: Fill, height: Fit, flow: Down, spacing: 6
            scope_choice_section := View {
                width: Fill, height: Fit
                scope_choice := PermissionChoices {
                    labels: ["Only the rooms and spaces I choose", "All rooms and spaces, including future ones"]
                }
            }
            target_checklist := View {
                width: Fill, height: Fit, flow: Down, spacing: 6
                target_filter := RobrixTextInput { width: Fill, empty_text: "Find rooms or spaces…" }
                targets := PortalList {
                    width: Fill, height: 168, flow: Down
                    target := RobrixSettingsCheckBox {}
                }
                target_empty := mod.widgets.PermissionOptionLabel { visible: false }
            }
        }
        duration_section := View {
            width: Fill, height: Fit, flow: Down, spacing: 8
            margin: Inset{top: 8}
            SettingsItemLabel { text: "Keep this permission" }
            duration_choice := PermissionChoices {
                labels: ["Until Robrix closes", "Until I change it"]
            }
            session_origin_section := View {
                visible: false
                width: Fill, height: Fit, flow: Down, spacing: 6
                SettingsItemLabel { text: "Room that ends this session" }
                ScrollYView {
                    width: Fill, height: 148
                    session_origin := PermissionChoices {}
                }
                mod.widgets.PermissionOptionLabel { text: "Closing this room ends the permission. The rooms it applies to are selected above." }
            }
        }
        network_section := View {
            visible: false
            width: Fill, height: Fit, flow: Down, spacing: 8
            margin: Inset{top: 8}
            SettingsItemLabel { text: "Allowed websites" }
            network_summary := mod.widgets.PermissionOptionLabel {}
            change_network := RobrixNeutralIconButton {
                text: "Change websites…"
                icon_walk: Walk{width: 0, height: 0, margin: 0}
            }
            network_settings := View {
                visible: false
                width: Fill, height: Fit, flow: Down, spacing: 8
                network_choice := PermissionChoices {
                    labels: ["Only this web address", "This website", "This domain and its subdomains"]
                }
                more_network := RobrixNeutralIconButton {
                    text: "More website options…"
                    icon_walk: Walk{width: 0, height: 0, margin: 0}
                }
                advanced_network := View {
                    visible: false
                    width: Fill, height: Fit, flow: Down, spacing: 8
                    network_advanced_choice := PermissionChoices {
                        selected_item: 2
                        labels: ["This host on any port", "Any website"]
                    }
                }
                network_value_section := View {
                    width: Fill, height: Fit
                    network_value := RobrixTextInput { width: Fill, empty_text: "https://example.com/page" }
                }
            }
            network_warning := RoundedView {
                width: Fill, height: Fit, padding: 12
                draw_bg +: { color: (COLOR_BG_PREVIEW), border_radius: 4.0 }
                mod.widgets.PermissionOptionLabel {
                    text: "Internet access can let this mini-app or agent send messages, files, or other local data it can access off this device to websites and online services. What is shared depends on what it does. Your data-sharing rules still apply."
                }
            }
        }
    }

    mod.widgets.MiniAppPermissionPrompt = set_type_default() do #(MiniAppPermissionPrompt::register_widget(vm)) {
        ..mod.widgets.SmallModal

        width: Fill { max: 640 }
        height: Fill { max: 760 }
        padding: 20
        scroll_bars: ScrollBars { show_scroll_x: false, show_scroll_y: false }

        prompt_title := ModalTitle { margin: Inset{bottom: 14} }
        prompt_content := ScrollYView {
            width: Fill, height: Fill, flow: Down, spacing: 12
            padding: Inset{right: 8}
            prompt_context := mod.widgets.PermissionOptionLabel {}
            prompt_blurb := ModalBody {}
            prompt_reason := ModalBody {
                draw_text +: { text_style: SETTINGS_REGULAR_TEXT_STYLE {}, color: (MESSAGE_TEXT_COLOR) }
            }

            // App-authored tool text stays plain and inert, with no links or markup.
            prompt_tool := RoundedView {
                width: Fill, height: Fit, visible: false, flow: Down, padding: 12, spacing: 8
                draw_bg +: { color: (COLOR_BG_PREVIEW), border_radius: 4.0 }
                SettingsItemLabel {
                    width: Fill, flow: Flow.Right{wrap: true}
                    text: "Tool details provided by the mini-app"
                }
                tool_label := mod.widgets.PermissionOptionLabel {}
            }
            remember_button := RobrixNeutralIconButton {
                text: "Remember this permission…"
                icon_walk: Walk{width: 0, height: 0, margin: 0}
            }
            remember_settings := View {
                visible: false
                width: Fill, height: Fit, flow: Down, spacing: 12
                LineH { height: 1 }
                scope_editor := mod.widgets.PermissionScopeEditor {}
                mod.widgets.PermissionOptionLabel {
                    text: "Room and space protections always take priority. You can change this permission later in Mini Apps."
                }
                allow_button := RobrixPositiveIconButton {
                    padding: 12,
                    draw_icon +: { svg: (ICON_CHECKMARK) }
                    icon_walk: Walk{width: 14, height: 14, margin: Inset{left: -2, right: -1}}
                    text: "Allow and remember"
                }
            }
            LineH { height: 1 }
            deny_button := RobrixNegativeIconButton {
                icon_walk: Walk{width: 0, height: 0, margin: 0}
                text: "Block in all rooms"
            }
            mod.widgets.PermissionOptionLabel {
                text: "Blocks this permission for this mini-app or agent in every room."
            }
        }

        ModalButtonsRow {
            spacing: 8, padding: Inset{top: 16, bottom: 0}
            allow_once_button := RobrixPositiveIconButton {
                padding: 12,
                icon_walk: Walk{width: 0, height: 0, margin: 0}
                text: "Allow once"
            }
            not_now_button := RobrixNeutralIconButton {
                padding: 12,
                icon_walk: Walk{width: 0, height: 0, margin: 0}
                text: "Not now"
            }
        }
    }
}

/// The app-authored tool text shown verbatim on an `mcp-tools` prompt — the
/// exact name, description and arguments that would enter the model's
/// context.
#[derive(Clone, Debug)]
pub struct ToolPreview {
    pub name: String,
    pub description: String,
    pub args: Vec<(String, String, String)>,
}

/// What the prompt modal needs to display one request.
pub struct PromptInfo {
    pub app_name: String,
    pub app_icon: String,
    pub perm: Permission,
    /// The app author's own `why-<perm>` reason, if declared.
    pub reason: Option<String>,
    /// The specific ability that triggered the ask (its catalog title), when
    /// a parked request identifies one.
    pub capability: Option<String>,
    /// Whether the asker is a room's AI agent (vs an installed mini-app):
    /// changes the wording of the blurb and reason lines.
    pub agent: bool,
    /// For an `mcp-tools` prompt: the app-authored tool text under review.
    pub tool: Option<ToolPreview>,
    /// The actual target of this request, which can differ from its source room.
    pub room_id: Option<String>,
    /// Closing this room expires a RoomSession grant.
    pub origin_room_id: Option<String>,
    pub network_url: Option<String>,
    /// A concrete request can be replayed once; subscriptions need a duration.
    pub can_allow_once: bool,
}

/// The user's answer, emitted as a global action for the runtime to apply.
#[derive(Clone, Debug, Default)]
pub enum PermissionPromptAction {
    AllowScoped { scope: RoomScope, duration: GrantDuration, network: Option<NetworkScope> },
    /// Authorizes only the single parked request.
    AllowOnce,
    Deny,
    /// Nothing persists; this (app, permission) stops asking for the session.
    NotNow,
    #[default]
    None,
}

#[derive(Script, ScriptHook, Widget)]
pub struct MiniAppPermissionPrompt {
    #[deref] view: View,
    #[rust] remember_expanded: bool,
}

impl Widget for MiniAppPermissionPrompt {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);

        if let Event::Actions(actions) = event {
            if self.view.button(cx, ids!(remember_button)).clicked(actions) {
                self.set_remember_expanded(cx, !self.remember_expanded);
            }
            let editor = self.view.permission_scope_editor(cx, ids!(scope_editor));
            if matches!(actions.find_widget_action(editor.widget_uid()).cast(), PermissionScopeEditorAction::Changed) {
                self.refresh_allow_button(cx);
            }
            if self.remember_expanded && self.view.button(cx, ids!(allow_button)).clicked(actions) {
                match self.view.permission_scope_editor(cx, ids!(scope_editor)).selection() {
                    Ok(selection) => cx.action(PermissionPromptAction::AllowScoped {
                        scope: selection.scope, duration: selection.duration, network: selection.network,
                    }),
                    Err(error) => enqueue_popup_notification(error, PopupKind::Warning, Some(5.0)),
                }
            } else if self.view.button(cx, ids!(allow_once_button)).clicked(actions) {
                cx.action(PermissionPromptAction::AllowOnce);
            } else if self.view.button(cx, ids!(deny_button)).clicked(actions) {
                cx.action(PermissionPromptAction::Deny);
            } else if self.view.button(cx, ids!(not_now_button)).clicked(actions) {
                cx.action(PermissionPromptAction::NotNow);
            }
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)
    }
}

impl MiniAppPermissionPrompt {
    fn set_remember_expanded(&mut self, cx: &mut Cx, expanded: bool) {
        self.remember_expanded = expanded;
        let max_height = if expanded { 760.0 } else { 620.0 };
        self.view.walk.height = Size::Fill {
            weight: 100.0, basis: FitBound::Abs(0.0), shrink: 0.0,
            min: None, max: Some(max_height),
        };
        self.view.widget(cx, ids!(remember_settings)).set_visible(cx, expanded);
        self.view.button(cx, ids!(remember_button)).set_text(cx, if expanded {
            "Hide remembered permission settings"
        } else { "Remember this permission…" });
        self.view.redraw(cx);
    }

    fn refresh_allow_button(&self, cx: &mut Cx) {
        let valid = self.view.permission_scope_editor(cx, ids!(scope_editor)).selection().is_ok();
        self.view.button(cx, ids!(allow_button)).set_enabled(cx, valid);
        self.view.widget(cx, ids!(allow_button)).set_disabled(cx, !valid);
    }
}

impl MiniAppPermissionPromptRef {
    /// Populates the prompt for the given request.
    pub fn show(&self, cx: &mut Cx, info: &PromptInfo) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.view.view(cx, ids!(prompt_content)).set_scroll_pos(cx, Vec2d::default());
        inner.view.permission_scope_editor(cx, ids!(scope_editor)).configure(
            cx, info.room_id.as_deref(), info.origin_room_id.as_deref(),
            info.network_url.as_deref(), info.perm == Permission::Network, false,
        );
        inner.set_remember_expanded(cx, !info.can_allow_once);
        let asked = info.capability.as_deref().unwrap_or(if info.perm == Permission::Network { "Access the internet" } else { info.perm.title() });
        inner.view.label(cx, ids!(prompt_title)).set_text(cx, &format!(
            "{} \"{}\" wants to: {}",
            info.app_icon, info.app_name, asked,
        ));
        let editor = inner.view.permission_scope_editor(cx, ids!(scope_editor));
        let mut context = Vec::new();
        if let Some(room) = info.room_id.as_deref() {
            let name = editor.borrow().and_then(|editor| editor.targets.iter()
                .find(|(id, _, _)| id == room).map(|(_, name, _)| name.clone()))
                .unwrap_or_else(|| room.to_string());
            context.push(format!("In {name}"));
        }
        if let Some(url) = info.network_url.as_deref() {
            context.push(format!("Requested web address: {url}"));
        }
        inner.view.label(cx, ids!(prompt_context)).set_text(cx, &context.join("\n"));
        inner.view.widget(cx, ids!(prompt_context)).set_visible(cx, !context.is_empty());
        inner.view.widget(cx, ids!(allow_once_button)).set_visible(cx, info.can_allow_once);
        let once_blurb = if info.perm == Permission::Network { "" }
        else if info.can_allow_once { " Allow once applies only to this request." }
        else { " This ongoing permission needs a room scope and duration. Choose them below." };
        // Keep the internet warning before the choices, including Allow once.
        // The management editor also shows it next to its website controls.
        inner.view.widget(cx, ids!(scope_editor.network_warning)).set_visible(cx, false);
        let description = if info.perm == Permission::Network {
            "This mini-app or agent may send room messages, files or other local data off this device to online services, depending on what it does."
        } else { info.perm.blurb() };
        let blurb = format!("{description}{once_blurb}");
        inner.view.label(cx, ids!(prompt_blurb)).set_text(cx, &blurb);
        let reason_text = match (info.agent, info.reason.as_deref()) {
            (true, Some(reason)) => reason.to_string(),
            (true, None) => String::from("It needs this to answer your messages in this room."),
            (false, Some(reason)) => format!("The app's stated reason: \"{reason}\""),
            (false, None) => String::from("The app gave no reason for needing this."),
        };
        inner.view.label(cx, ids!(prompt_reason)).set_text(cx, &reason_text);
        // The app's own tool text, verbatim. The user reviews exactly what the
        // model will read, so this is never reformatted.
        match &info.tool {
            Some(tool) => {
                let mut text = format!("Tool name: {}\n\n{}", tool.name, tool.description);
                if !tool.args.is_empty() {
                    text.push_str("\n\nArguments:");
                    for (name, ty, desc) in &tool.args {
                        text.push_str(&format!("\n  • {name} ({ty})"));
                        if !desc.trim().is_empty() {
                            text.push_str(&format!(" — {desc}"));
                        }
                    }
                }
                inner.view.label(cx, ids!(tool_label)).set_text(cx, &text);
                inner.view.view(cx, ids!(prompt_tool)).set_visible(cx, true);
            }
            None => inner.view.view(cx, ids!(prompt_tool)).set_visible(cx, false),
        }
        inner.refresh_allow_button(cx);
        inner.view.redraw(cx);
    }
}

/// Shared by the request prompt and the permission/policy editors.
pub(crate) struct PermissionSelection {
    pub scope: RoomScope,
    pub duration: GrantDuration,
    pub network: Option<NetworkScope>,
    pub origin_room: Option<String>,
}

#[derive(Clone, Debug, Default)]
enum PermissionScopeEditorAction {
    Changed,
    #[default]
    None,
}

#[derive(Script, ScriptHook, Widget)]
pub struct PermissionScopeEditor {
    #[deref] view: View,
    #[rust] targets: Vec<(String, String, bool)>,
    #[rust] filtered: Vec<usize>,
    #[rust] rooms: BTreeSet<String>,
    #[rust] spaces: BTreeSet<String>,
    #[rust] all_rooms: bool,
    #[rust] scope_choice_valid: bool,
    #[rust] targets_expanded: bool,
    #[rust] room_policy: bool,
    #[rust] origin_room: Option<String>,
    #[rust] choose_origin: bool,
    #[rust] session_rooms: Vec<String>,
    #[rust] session_origin_index: usize,
    #[rust] duration: usize,
    #[rust] network: bool,
    #[rust] network_kind: usize,
    #[rust] network_value: String,
    #[rust] network_expanded: bool,
    #[rust] advanced_network_expanded: bool,
}

impl Widget for PermissionScopeEditor {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        let Event::Actions(actions) = event else { return };
        let mut changed = false;
        if let Some(index) = self.view.permission_choices(cx, ids!(scope_choice)).changed(actions) {
            changed = true;
            self.scope_choice_valid = index < 2;
            self.all_rooms = index == 1;
            self.update_target_visibility(cx);
        }
        if self.view.button(cx, ids!(choose_targets)).clicked(actions) {
            changed = true;
            self.targets_expanded = !self.targets_expanded;
            self.update_target_visibility(cx);
        }
        if let Some(index) = self.view.permission_choices(cx, ids!(duration_choice)).changed(actions) {
            changed = true;
            self.duration = index;
            self.view.widget(cx, ids!(session_origin_section)).set_visible(cx, self.choose_origin && index == 0);
        }
        if let Some(index) = self.view.permission_choices(cx, ids!(session_origin)).changed(actions) {
            changed = true;
            self.session_origin_index = index;
        }
        if let Some(index) = self.view.permission_choices(cx, ids!(network_choice)).changed(actions) {
            changed = true;
            self.network_kind = match index { 0 => 0, 1 => 1, 2 => 3, _ => usize::MAX };
            self.view.permission_choices(cx, ids!(network_advanced_choice)).set_selected_item(cx, usize::MAX);
            self.update_network_visibility(cx);
            self.update_network_summary(cx);
        }
        if let Some(index) = self.view.permission_choices(cx, ids!(network_advanced_choice)).changed(actions) {
            changed = true;
            self.network_kind = match index { 0 => 2, 1 => 4, _ => usize::MAX };
            self.view.permission_choices(cx, ids!(network_choice)).set_selected_item(cx, usize::MAX);
            self.update_network_visibility(cx);
            self.update_network_summary(cx);
        }
        if self.view.button(cx, ids!(change_network)).clicked(actions) {
            self.network_expanded = !self.network_expanded;
            self.update_network_visibility(cx);
        }
        if self.view.button(cx, ids!(more_network)).clicked(actions) {
            self.advanced_network_expanded = !self.advanced_network_expanded;
            self.update_network_visibility(cx);
        }
        if let Some(value) = self.view.text_input(cx, ids!(network_value)).changed(actions) {
            changed = true;
            self.network_value = value;
            self.update_network_summary(cx);
        }
        if let Some(filter) = self.view.text_input(cx, ids!(target_filter)).changed(actions) {
            changed = true;
            let filter = filter.to_lowercase();
            self.filtered = self.targets.iter().enumerate()
                .filter(|(_, (id, name, _))| name.to_lowercase().contains(&filter) || id.contains(&filter))
                .map(|(index, _)| index).collect();
            self.view.portal_list(cx, ids!(targets)).set_first_id_and_scroll(0, 0.0);
        }
        for (row_index, row) in self.view.portal_list(cx, ids!(targets)).items_with_actions(actions) {
            if let Some(checked) = row.as_check_box().changed(actions)
                && let Some(&target_index) = self.filtered.get(row_index)
            {
                changed = true;
                let (id, _, is_space) = &self.targets[target_index];
                let selected = if *is_space { &mut self.spaces } else { &mut self.rooms };
                if checked { selected.insert(id.clone()); } else { selected.remove(id); }
            }
        }
        if changed {
            self.update_summary(cx);
            self.view.redraw(cx);
            cx.widget_action(self.widget_uid(), PermissionScopeEditorAction::Changed);
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        while let Some(widget) = self.view.draw_walk(cx, scope, walk).step() {
            if let Some(mut list) = widget.as_portal_list().borrow_mut() {
                list.set_item_range(cx, 0, self.filtered.len());
                while let Some(index) = list.next_visible_item(cx) {
                    let Some(target_index) = self.filtered.get(index) else { continue };
                    let (id, name, is_space) = &self.targets[*target_index];
                    let row = list.item(cx, index, id!(target));
                    let checkbox = row.as_check_box();
                    row.set_text(cx, &format!("{} · {}", name, if *is_space { "Space and its rooms" } else { "Room" }));
                    let selected = if *is_space { &self.spaces } else { &self.rooms };
                    checkbox.set_active(cx, selected.contains(id), Animate::No);
                    row.draw_all(cx, scope);
                }
            }
        }
        DrawStep::done()
    }
}

impl PermissionScopeEditor {
    fn update_target_visibility(&self, cx: &mut Cx) {
        self.view.widget(cx, ids!(choose_targets)).set_visible(cx, !self.room_policy);
        self.view.widget(cx, ids!(selected_targets)).set_visible(cx, self.targets_expanded);
        self.view.widget(cx, ids!(target_checklist)).set_visible(cx, !self.all_rooms);
        self.view.button(cx, ids!(choose_targets)).set_text(cx, if self.targets_expanded {
            "Done choosing rooms"
        } else { "Change rooms…" });
    }

    fn update_network_visibility(&mut self, cx: &mut Cx) {
        self.view.widget(cx, ids!(network_settings)).set_visible(cx, self.network_expanded);
        self.view.widget(cx, ids!(advanced_network)).set_visible(cx, self.advanced_network_expanded);
        self.view.widget(cx, ids!(network_value_section)).set_visible(cx, self.network_kind != 4);
        self.view.button(cx, ids!(change_network)).set_text(cx, if self.network_expanded {
            "Done choosing websites"
        } else { "Change websites…" });
        self.view.button(cx, ids!(more_network)).set_text(cx, if self.advanced_network_expanded {
            "Hide advanced website options"
        } else { "More website options…" });
        self.view.redraw(cx);
    }

    fn update_summary(&self, cx: &mut Cx) {
        let selected = self.targets.iter().filter(|(id, _, space)| {
            if *space { self.spaces.contains(id) } else { self.rooms.contains(id) }
        }).map(|(_, name, space)| if *space { format!("{name} (space)") } else { name.clone() }).collect::<Vec<_>>();
        let text = if self.all_rooms {
            "Applies in every room and space, including ones you join later.".to_string()
        } else if selected.is_empty() {
            "Choose at least one room or space. Selecting a space includes its nested rooms.".to_string()
        } else {
            let mut text = format!("Selected: {}", selected.join(", "));
            if !self.spaces.is_empty() { text.push_str(". Spaces include their nested rooms."); }
            text
        };
        self.view.label(cx, ids!(target_summary)).set_text(cx, &text);
        self.view.widget(cx, ids!(target_empty)).set_visible(cx, self.filtered.is_empty());
        self.view.label(cx, ids!(target_empty)).set_text(cx, if self.targets.is_empty() {
            "No rooms or spaces are loaded yet. Open a room and try again."
        } else { "No rooms or spaces match your search." });
    }

    fn network_scope(&self) -> Result<Option<NetworkScope>, String> {
        if !self.network { return Ok(None) }
        if self.network_kind == 4 { return Ok(Some(NetworkScope::AllHosts)) }
        let kind = match self.network_kind {
            0 => NetworkScopeKind::ExactUrl,
            1 => NetworkScopeKind::Origin,
            2 => NetworkScopeKind::Host,
            3 => NetworkScopeKind::Domain,
            _ => return Err("Choose which websites to allow.".into()),
        };
        NetworkScope::from_url(self.network_value.trim(), kind).map(Some)
    }

    fn update_network_summary(&self, cx: &mut Cx) {
        let text = match self.network_scope() {
            Ok(Some(network)) => match &network {
                NetworkScope::ExactUrl(url) => format!("Only this address: {url}"),
                NetworkScope::Origin(origin) => format!("Every page at {origin}. Other protocols, ports, and subdomains need separate permission."),
                NetworkScope::Host(host) => format!("{host} over HTTP or HTTPS, on any port. Subdomains need separate permission."),
                NetworkScope::Domain(domain) => format!("{domain} and all its subdomains, over HTTP or HTTPS on any port."),
                NetworkScope::AllHosts => "Any website, including sites this mini-app or agent has not contacted yet.".into(),
            },
            Ok(None) => String::new(),
            Err(error) => error,
        };
        self.view.label(cx, ids!(network_summary)).set_text(cx, &text);
    }
}

impl PermissionScopeEditorRef {
    pub(crate) fn configure(
        &self, cx: &mut Cx, room_id: Option<&str>, origin_room: Option<&str>,
        network_url: Option<&str>, network: bool, policy: bool,
    ) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.targets = if cx.has_global::<RoomsListRef>() {
            cx.get_global::<RoomsListRef>().permission_targets()
        } else { Vec::new() };
        inner.rooms.clear();
        inner.spaces.clear();
        if let Some(room_id) = room_id {
            inner.rooms.insert(room_id.to_string());
            if !inner.targets.iter().any(|(id, _, _)| id == room_id) {
                inner.targets.insert(0, (room_id.to_string(), room_id.to_string(), false));
            }
        }
        inner.filtered = (0..inner.targets.len()).collect();
        inner.origin_room = origin_room.map(str::to_owned);
        inner.choose_origin = false;
        inner.session_rooms.clear();
        inner.session_origin_index = 0;
        inner.view.widget(cx, ids!(session_origin_section)).set_visible(cx, false);
        inner.duration = 0;
        inner.network = network;
        inner.network_kind = 0;
        inner.network_value = network_url.unwrap_or("").to_string();
        inner.network_expanded = network && network_url.is_none();
        inner.advanced_network_expanded = false;
        inner.room_policy = policy;
        inner.all_rooms = room_id.is_none() && !policy;
        inner.scope_choice_valid = true;
        inner.view.permission_choices(cx, ids!(scope_choice)).set_selected_item(cx, usize::from(inner.all_rooms));
        inner.view.widget(cx, ids!(scope_choice_section)).set_visible(cx, !policy);
        inner.targets_expanded = !inner.all_rooms && (policy || room_id.is_none());
        inner.update_target_visibility(cx);
        inner.view.text_input(cx, ids!(target_filter)).set_text(cx, "");
        inner.view.portal_list(cx, ids!(targets)).set_first_id_and_scroll(0, 0.0);
        let mut durations = Vec::new();
        if let Some(origin) = origin_room {
            let name = inner.targets.iter().find(|(id, _, _)| id == origin).map(|(_, name, _)| name.as_str()).unwrap_or(origin);
            durations.push(format!("Until {name} closes"));
        }
        durations.extend(["Until Robrix closes".to_string(), "Until I change it".to_string()]);
        inner.view.permission_choices(cx, ids!(duration_choice)).set_labels(cx, durations);
        inner.view.permission_choices(cx, ids!(duration_choice)).set_selected_item(cx, 0);
        inner.view.widget(cx, ids!(duration_section)).set_visible(cx, !policy);
        inner.view.widget(cx, ids!(network_section)).set_visible(cx, network);
        inner.view.widget(cx, ids!(network_warning)).set_visible(cx, true);
        inner.view.permission_choices(cx, ids!(network_choice)).set_selected_item(cx, 0);
        inner.view.permission_choices(cx, ids!(network_advanced_choice)).set_selected_item(cx, usize::MAX);
        inner.view.text_input(cx, ids!(network_value)).set_text(cx, network_url.unwrap_or(""));
        inner.update_network_visibility(cx);
        inner.update_summary(cx);
        inner.update_network_summary(cx);
        inner.view.redraw(cx);
    }

    pub(crate) fn selection(&self) -> Result<PermissionSelection, String> {
        let Some(inner) = self.borrow() else { return Err("Permission editor is unavailable.".into()) };
        if !inner.scope_choice_valid { return Err("Choose which rooms and spaces to allow.".into()) }
        let scope = if inner.all_rooms { RoomScope::AllRooms } else {
            if inner.rooms.is_empty() && inner.spaces.is_empty() {
                return Err("Select at least one room or space.".into());
            }
            RoomScope::Selection { rooms: inner.rooms.iter().cloned().collect(), spaces: inner.spaces.iter().cloned().collect() }
        };
        let duration = match (inner.origin_room.is_some() || inner.choose_origin, inner.duration) {
            (true, 0) => GrantDuration::RoomSession,
            (true, 1) | (false, 0) => GrantDuration::RobrixSession,
            (true, 2) | (false, 1) => GrantDuration::Always,
            _ => return Err("Choose how long to keep this permission.".into()),
        };
        let origin_room = if duration == GrantDuration::RoomSession && inner.choose_origin {
            Some(inner.session_rooms.get(inner.session_origin_index).cloned().ok_or("Choose the room that ends this permission when it closes.")?)
        } else { inner.origin_room.clone() };
        Ok(PermissionSelection { scope, duration, network: inner.network_scope()?, origin_room })
    }

    /// Management may grant a room session before the mini-app is opened there.
    pub(crate) fn enable_room_session_picker(&self, cx: &mut Cx) {
        let Some(mut inner) = self.borrow_mut() else { return };
        if inner.origin_room.is_some() { return }
        let rooms = inner.targets.iter().filter(|(_, _, space)| !space).cloned().collect::<Vec<_>>();
        if rooms.is_empty() { return }
        inner.choose_origin = true;
        inner.session_rooms = rooms.iter().map(|(id, _, _)| id.clone()).collect();
        inner.view.permission_choices(cx, ids!(session_origin)).set_labels(cx, rooms.into_iter().map(|(_, name, _)| name).collect());
        inner.view.permission_choices(cx, ids!(session_origin)).set_selected_item(cx, 0);
        inner.view.permission_choices(cx, ids!(duration_choice)).set_labels(cx, vec![
            "Until a selected room closes".into(), "Until Robrix closes".into(), "Until I change it".into(),
        ]);
        inner.duration = 1;
        inner.view.permission_choices(cx, ids!(duration_choice)).set_selected_item(cx, 1);
    }

    pub(crate) fn set_scope(&self, cx: &mut Cx, scope: &RoomScope) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.all_rooms = matches!(scope, RoomScope::AllRooms);
        inner.scope_choice_valid = true;
        inner.rooms.clear();
        inner.spaces.clear();
        if let RoomScope::Selection { rooms, spaces } = scope {
            inner.rooms.extend(rooms.iter().cloned());
            inner.spaces.extend(spaces.iter().cloned());
            for (ids, space) in [(rooms, false), (spaces, true)] {
                for id in ids {
                    if !inner.targets.iter().any(|(target, _, _)| target == id) {
                        inner.targets.push((id.clone(), id.clone(), space));
                    }
                }
            }
            inner.filtered = (0..inner.targets.len()).collect();
        }
        inner.view.permission_choices(cx, ids!(scope_choice)).set_selected_item(cx, usize::from(inner.all_rooms));
        inner.targets_expanded = inner.room_policy || (!inner.all_rooms && inner.rooms.is_empty() && inner.spaces.is_empty());
        inner.update_target_visibility(cx);
        inner.update_summary(cx);
        inner.view.redraw(cx);
    }
}

pub(crate) fn network_scope_label(scope: &NetworkScope) -> String {
    match scope {
        NetworkScope::ExactUrl(url) => format!("Only this address: {url}"),
        NetworkScope::Origin(origin) => format!("Website: {origin} (same protocol and port)"),
        NetworkScope::Host(host) => format!("Host: {host} (HTTP/S, any port)"),
        NetworkScope::Domain(domain) => format!("Domain: {domain} and subdomains (HTTP/S, any port)"),
        NetworkScope::AllHosts => "Any website".into(),
    }
}

pub(crate) fn duration_label(duration: GrantDuration) -> &'static str {
    match duration {
        GrantDuration::RoomSession => "Until the starting room closes",
        GrantDuration::RobrixSession => "Until Robrix closes",
        GrantDuration::Always => "Until you change it",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor() -> (Cx, PermissionScopeEditorRef) {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let widget = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            crate::shared::script_mod(vm);
            super::super::permission_choices::script_mod(vm);
            super::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.PermissionScopeEditor {} });
            WidgetRef::script_from_value(vm, value)
        });
        (cx, widget.as_permission_scope_editor())
    }

    #[test]
    fn selection_keeps_target_and_source_room_distinct_and_rejects_empty_scope() {
        let (mut cx, editor) = editor();
        editor.configure(&mut cx, Some("!target:example.org"), Some("!source:example.org"), None, false, false);
        let selection = editor.selection().unwrap();
        assert_eq!(selection.scope, RoomScope::room("!target:example.org"));
        assert_eq!(selection.origin_room.as_deref(), Some("!source:example.org"));
        assert_eq!(selection.duration, GrantDuration::RoomSession);
        editor.set_scope(&mut cx, &RoomScope::Selection { rooms: Vec::new(), spaces: Vec::new() });
        assert!(editor.selection().is_err(), "an empty checklist must never broaden to all rooms");
    }

    #[test]
    fn network_editor_requires_valid_destination_except_explicit_all_internet_choice() {
        let (mut cx, editor) = editor();
        editor.configure(&mut cx, Some("!room:example.org"), None, None, true, false);
        assert!(editor.selection().is_err());
        editor.borrow_mut().unwrap().network_kind = 4;
        assert_eq!(editor.selection().unwrap().network, Some(NetworkScope::AllHosts));
        editor.configure(&mut cx, Some("!room:example.org"), None, Some("https://Example.org:443/path"), true, false);
        editor.borrow_mut().unwrap().network_kind = 1;
        assert_eq!(editor.selection().unwrap().network, Some(NetworkScope::Origin("https://example.org".into())));
        editor.configure(&mut cx, Some("!other:example.org"), None, None, false, false);
        let selection = editor.selection().unwrap();
        assert_eq!(selection.scope, RoomScope::room("!other:example.org"));
        assert_eq!(selection.network, None, "the previous prompt's internet selection must be discarded");
        assert_eq!(selection.duration, GrantDuration::RobrixSession);
    }

    #[test]
    fn website_choices_only_broaden_after_an_explicit_selection_and_reset_for_each_request() {
        let (mut cx, editor) = editor();
        let url = "https://example.org/page";
        editor.configure(&mut cx, Some("!room:example.org"), None, Some(url), true, false);
        let exact = Some(NetworkScope::ExactUrl(url.into()));
        assert_eq!(editor.selection().unwrap().network, exact);
        assert!(!editor.borrow().unwrap().view.widget(&cx, ids!(network_settings)).visible());
        assert!(!editor.borrow().unwrap().view.widget(&cx, ids!(advanced_network)).visible());
        for button in [ids!(change_network), ids!(more_network)] {
            let uid = editor.borrow().unwrap().view.button(&cx, button).widget_uid();
            let click = cx.capture_actions(|cx| cx.widget_action(uid, ButtonAction::Clicked(Default::default())));
            editor.borrow_mut().unwrap().handle_event(&mut cx, &Event::Actions(click), &mut Scope::empty());
            assert_eq!(editor.selection().unwrap().network, exact, "opening choices never changes the destination scope");
        }
        let common = editor.borrow().unwrap().view.permission_choices(&cx, ids!(network_choice)).widget_uid();
        let advanced = editor.borrow().unwrap().view.permission_choices(&cx, ids!(network_advanced_choice)).widget_uid();
        for (uid, index, expected) in [
            (common, 2, NetworkScope::Domain("example.org".into())),
            (advanced, 0, NetworkScope::Host("example.org".into())),
            (advanced, 1, NetworkScope::AllHosts),
            (common, 0, NetworkScope::ExactUrl(url.into())),
        ] {
            let change = cx.capture_actions(|cx| cx.widget_action(uid, PermissionChoicesAction::Changed(index)));
            editor.borrow_mut().unwrap().handle_event(&mut cx, &Event::Actions(change), &mut Scope::empty());
            assert_eq!(editor.selection().unwrap().network, Some(expected));
        }
        let change = cx.capture_actions(|cx| cx.widget_action(common, PermissionChoicesAction::Changed(99)));
        editor.borrow_mut().unwrap().handle_event(&mut cx, &Event::Actions(change), &mut Scope::empty());
        assert_eq!(editor.selection().unwrap().network, exact, "an invalid choice must preserve the exact destination");
        editor.configure(&mut cx, Some("!room:example.org"), None, Some(url), true, false);
        assert_eq!(editor.selection().unwrap().network, exact);
        assert!(!editor.borrow().unwrap().view.widget(&cx, ids!(advanced_network)).visible());
    }

    #[test]
    fn room_selection_expands_when_needed_and_resets_between_editors() {
        let (mut cx, editor) = editor();
        editor.configure(&mut cx, Some("!room:example.org"), None, None, false, false);
        assert!(!editor.borrow().unwrap().targets_expanded);
        assert!(editor.borrow().unwrap().view.widget(&cx, ids!(scope_choice_section)).visible());
        assert!(!editor.borrow().unwrap().view.widget(&cx, ids!(selected_targets)).visible());
        assert!(editor.borrow().unwrap().view.label(&cx, ids!(target_summary)).text().contains("!room:example.org"));

        editor.configure(&mut cx, None, None, None, false, true);
        assert!(editor.borrow().unwrap().targets_expanded);
        assert!(!editor.borrow().unwrap().view.widget(&cx, ids!(scope_choice_section)).visible());
        assert!(editor.borrow().unwrap().view.widget(&cx, ids!(selected_targets)).visible());
        assert!(editor.selection().is_err());
        editor.set_scope(&mut cx, &RoomScope::room("!room:example.org"));
        assert!(editor.borrow().unwrap().view.widget(&cx, ids!(selected_targets)).visible(),
            "a room-rule target step opens its checklist directly, including when editing an existing rule");
        assert!(!editor.borrow().unwrap().view.widget(&cx, ids!(choose_targets)).visible());

        editor.configure(&mut cx, None, None, None, false, false);
        assert!(editor.borrow().unwrap().view.widget(&cx, ids!(scope_choice_section)).visible());
        assert!(!editor.borrow().unwrap().view.widget(&cx, ids!(selected_targets)).visible());
        assert!(editor.borrow().unwrap().view.label(&cx, ids!(target_summary)).text().contains("including ones you join later"));
    }

    #[test]
    fn invalid_duration_and_network_choices_never_broaden_permission() {
        let (mut cx, editor) = editor();
        editor.configure(&mut cx, Some("!room:example.org"), None, Some("https://example.org/page"), true, false);
        editor.borrow_mut().unwrap().network_kind = 5;
        assert!(editor.selection().is_err(), "unknown website scope must never become all websites");
        editor.borrow_mut().unwrap().network_kind = 0;
        editor.borrow_mut().unwrap().duration = 2;
        assert!(editor.selection().is_err(), "unknown duration must never become a permanent grant");
    }

    #[test]
    fn prompt_keeps_app_text_plain_and_resets_one_time_approval_visibility() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let prompt = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            crate::shared::script_mod(vm);
            super::super::permission_choices::script_mod(vm);
            super::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.MiniAppPermissionPrompt {} });
            WidgetRef::script_from_value(vm, value).as_mini_app_permission_prompt()
        });
        let mut info = PromptInfo {
            app_name: "Test mini-app".into(), app_icon: "".into(), perm: Permission::Network,
            reason: Some("<a href=\"https://example.org\">a claimed reason</a>".into()),
            capability: None, agent: false,
            tool: Some(ToolPreview {name: "example".into(), description: "<b>Plain tool text</b>".into(), args: Vec::new()}),
            room_id: Some("!room:example.org".into()), origin_room_id: None,
            network_url: Some("https://example.org/page".into()), can_allow_once: true,
        };
        prompt.show(&mut cx, &info);
        assert!(!prompt.borrow().unwrap().view.widget(&cx, ids!(remember_settings)).visible());
        assert!(prompt.borrow().unwrap().view.label(&cx, ids!(prompt_reason)).text().contains("<a href="));
        assert!(prompt.borrow().unwrap().view.label(&cx, ids!(tool_label)).text().contains("<b>Plain tool text</b>"));
        assert!(prompt.borrow().unwrap().view.widget(&cx, ids!(allow_once_button)).visible());
        assert!(prompt.borrow().unwrap().view.label(&cx, ids!(prompt_blurb)).text().contains("off this device"));
        let editor = prompt.borrow().unwrap().view.permission_scope_editor(&cx, ids!(scope_editor));
        assert!(!editor.borrow().unwrap().view.widget(&cx, ids!(network_warning)).visible(), "the prompt warning must appear only once");
        info.can_allow_once = false;
        info.tool = None;
        prompt.show(&mut cx, &info);
        assert!(!prompt.borrow().unwrap().view.widget(&cx, ids!(allow_once_button)).visible());
        assert!(!prompt.borrow().unwrap().view.widget(&cx, ids!(prompt_tool)).visible());
        assert!(prompt.borrow().unwrap().view.widget(&cx, ids!(remember_settings)).visible(), "an ongoing request must expose the required scope and duration");
        info.can_allow_once = true;
        prompt.show(&mut cx, &info);
        assert!(!prompt.borrow().unwrap().view.widget(&cx, ids!(remember_settings)).visible(), "a new one-time request starts with the simple decision");
    }

    #[test]
    fn scoped_approval_tracks_form_validity_without_disabling_one_time_approval() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let prompt = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            crate::shared::script_mod(vm);
            super::super::permission_choices::script_mod(vm);
            super::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.MiniAppPermissionPrompt {} });
            WidgetRef::script_from_value(vm, value).as_mini_app_permission_prompt()
        });
        let info = PromptInfo {
            app_name: "Test mini-app".into(), app_icon: String::new(), perm: Permission::Network,
            reason: None, capability: None, agent: false, tool: None,
            room_id: None, origin_room_id: None, network_url: None, can_allow_once: true,
        };
        prompt.show(&mut cx, &info);
        let remember = prompt.borrow().unwrap().view.button(&cx, ids!(remember_button)).widget_uid();
        let click = cx.capture_actions(|cx| cx.widget_action(remember, ButtonAction::Clicked(Default::default())));
        prompt.borrow_mut().unwrap().handle_event(&mut cx, &Event::Actions(click), &mut Scope::empty());
        assert!(prompt.borrow().unwrap().view.widget(&cx, ids!(remember_settings)).visible());
        let editor = prompt.borrow().unwrap().view.permission_scope_editor(&cx, ids!(scope_editor));
        let network_input = editor.borrow().unwrap().view.text_input(&cx, ids!(network_value)).widget_uid();
        let room_scope = editor.borrow().unwrap().view.permission_choices(&cx, ids!(scope_choice)).widget_uid();
        let allow = prompt.borrow().unwrap().view.widget(&cx, ids!(allow_button));
        let once = prompt.borrow().unwrap().view.widget(&cx, ids!(allow_once_button));
        assert!(allow.disabled(&cx));
        assert!(!once.disabled(&cx));

        // Dispatch the same child actions a user edit produces, then the editor's notification.
        for (text, disabled) in [("https://example.org/page", false), ("not a web address", true), ("https://example.org/page", false)] {
            let change = cx.capture_actions(|cx| cx.widget_action(network_input, TextInputAction::Changed(text.into())));
            let notify = cx.capture_actions(|cx| prompt.borrow_mut().unwrap().handle_event(cx, &Event::Actions(change), &mut Scope::empty()));
            prompt.borrow_mut().unwrap().handle_event(&mut cx, &Event::Actions(notify), &mut Scope::empty());
            assert_eq!(allow.disabled(&cx), disabled);
            assert!(!once.disabled(&cx));
        }
        for (index, disabled) in [(0, true), (1, false)] {
            let change = cx.capture_actions(|cx| cx.widget_action(room_scope, PermissionChoicesAction::Changed(index)));
            let notify = cx.capture_actions(|cx| prompt.borrow_mut().unwrap().handle_event(cx, &Event::Actions(change), &mut Scope::empty()));
            prompt.borrow_mut().unwrap().handle_event(&mut cx, &Event::Actions(notify), &mut Scope::empty());
            assert_eq!(allow.disabled(&cx), disabled, "an empty room selection cannot be approved");
        }
        prompt.show(&mut cx, &info);
        assert!(allow.disabled(&cx), "a new prompt must not keep the previous enabled state");
    }

}
