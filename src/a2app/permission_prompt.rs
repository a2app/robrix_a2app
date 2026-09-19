//! The runtime permission prompt for mini-apps: "«App» wants to «do X»",
//! with explicit scope and duration, Allow Once, Block Everywhere, and Not Now.
//!
//! Shown one at a time; the queue lives in [`crate::a2app::runtime`].

use makepad_widgets::*;
use std::collections::BTreeSet;
use a2app_core::permissions::{GrantDuration, NetworkScope, NetworkScopeKind, Permission, RoomScope};
use crate::home::rooms_list::RoomsListRef;
use crate::shared::popup_list::{enqueue_popup_notification, PopupKind};

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    mod.widgets.PermissionOptionLabel = Label {
        width: Fill, height: Fit, padding: 0, margin: 0
        flow: Flow.Right{wrap: true}
        draw_text +: { text_style: REGULAR_TEXT {font_size: 10.5}, color: (COLOR_TEXT) }
    }
    mod.widgets.PermissionDropDown = DropDownFlat {
        width: Fill, height: 32
        draw_text +: { text_style: REGULAR_TEXT {font_size: 10.5}, color: (COLOR_TEXT) }
        draw_bg +: { color: (COLOR_PRIMARY), border_color: (COLOR_DIVIDER), border_size: 1.0 }
    }
    mod.widgets.PermissionScopeEditor = set_type_default() do #(PermissionScopeEditor::register_widget(vm)) {
        width: Fill, height: Fit, flow: Down, spacing: 7
        mod.widgets.PermissionOptionLabel { text: "Where" }
        scope_choice := mod.widgets.PermissionDropDown {
            labels: ["Selected rooms and spaces", "All rooms"]
        }
        selected_targets := View {
            width: Fill, height: Fit, flow: Down, spacing: 4
            target_filter := RobrixTextInput { width: Fill, empty_text: "Find rooms or spaces…" }
            targets := PortalList {
                width: Fill, height: 150, flow: Down
                target := CheckBox {
                    width: Fill, height: 30
                    draw_text +: { text_style: REGULAR_TEXT {font_size: 10.5}, color: (COLOR_TEXT) }
                }
            }
            target_summary := mod.widgets.PermissionOptionLabel {}
        }
        duration_section := View {
            width: Fill, height: Fit, flow: Down, spacing: 4
            mod.widgets.PermissionOptionLabel { text: "For how long" }
            duration_choice := mod.widgets.PermissionDropDown {
                labels: ["Until Robrix closes", "Always"]
            }
            session_origin_section := View {
                visible: false
                width: Fill, height: Fit, flow: Down, spacing: 4
                mod.widgets.PermissionOptionLabel { text: "Source room for this session" }
                session_origin := mod.widgets.PermissionDropDown {}
                mod.widgets.PermissionOptionLabel { text: "Applies while the mini-app runs in this source room; ends when that room closes." }
            }
        }
        network_section := View {
            visible: false
            width: Fill, height: Fit, flow: Down, spacing: 4
            mod.widgets.PermissionOptionLabel { text: "Internet access" }
            network_choice := mod.widgets.PermissionDropDown {
                labels: ["Only this URL", "This site (scheme, host and port)", "This exact host (all ports)", "This domain and subdomains (all ports)", "All internet sites"]
            }
            network_value := RobrixTextInput { width: Fill, empty_text: "https://example.com/path" }
            network_summary := mod.widgets.PermissionOptionLabel {}
        }
    }

    mod.widgets.MiniAppPermissionPrompt = set_type_default() do #(MiniAppPermissionPrompt::register_widget(vm)) {
        ..mod.widgets.SmallModal

        // Wide enough for all four answer buttons on one row.
        width: Fill { max: 640 }

        prompt_glyph := Label {
            width: Fill, height: Fit
            align: Align{x: 0.5}
            margin: Inset{bottom: 10}
            draw_text +: {
                text_style: TITLE_TEXT {font_size: 28},
                color: #000
            }
        }
        prompt_title := ModalTitle {
            margin: Inset{bottom: 10}
        }
        prompt_blurb := ModalBody {}
        prompt_reason := ModalBody {
            margin: Inset{top: 10}
            draw_text +: {
                text_style: REGULAR_TEXT {font_size: 10.5},
                color: (MESSAGE_TEXT_COLOR)
            }
        }

        // The app-authored tool text, shown VERBATIM and rendered as inert
        // plain text: no markdown, no links, nothing the app can dress up.
        // This is exactly what would enter the model's context.
        prompt_tool := View {
            width: Fill, height: Fit
            visible: false
            flow: Down
            margin: Inset{top: 10}
            tool_label := Label {
                width: Fill, height: Fit
                padding: 8, margin: 0
                draw_text +: {
                    text_style: REGULAR_TEXT {font_size: 10.5},
                    color: #x1C274C
                }
            }
        }

        scope_editor := mod.widgets.PermissionScopeEditor {}

        ModalButtonsRow {
            align: Align{x: 0.5, y: 0.5}
            spacing: 8
            margin: Inset{top: 6}

            not_now_button := RobrixNeutralIconButton {
                padding: 12,
                icon_walk: Walk{width: 0, height: 0, margin: 0}
                text: "Not Now"
            }
            deny_button := RobrixNegativeIconButton {
                padding: 12,
                draw_icon +: { svg: (ICON_FORBIDDEN) }
                icon_walk: Walk{width: 14, height: 14, margin: Inset{left: -2, right: -1}}
                text: "Block everywhere"
            }
            allow_once_button := RobrixIconButton {
                padding: 12,
                icon_walk: Walk{width: 0, height: 0, margin: 0}
                text: "Allow Once"
            }
            allow_button := RobrixPositiveIconButton {
                padding: 12,
                draw_icon +: { svg: (ICON_CHECKMARK) }
                icon_walk: Walk{width: 14, height: 14, margin: Inset{left: -2, right: -1}}
                text: "Allow selected"
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
}

impl Widget for MiniAppPermissionPrompt {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);

        if let Event::Actions(actions) = event {
            if self.view.button(cx, ids!(allow_button)).clicked(actions) {
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

impl MiniAppPermissionPromptRef {
    /// Populates the prompt for the given request.
    pub fn show(&self, cx: &mut Cx, info: &PromptInfo) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.view.permission_scope_editor(cx, ids!(scope_editor)).configure(
            cx, info.room_id.as_deref(), info.origin_room_id.as_deref(),
            info.network_url.as_deref(), info.perm == Permission::Network, false,
        );
        inner.view.label(cx, ids!(prompt_glyph)).set_text(cx, info.perm.glyph());
        let asked = info.capability.as_deref().unwrap_or(info.perm.title());
        inner.view.label(cx, ids!(prompt_title)).set_text(cx, &format!(
            "{} \"{}\" wants to: {}",
            info.app_icon, info.app_name, asked,
        ));
        inner.view.widget(cx, ids!(allow_once_button)).set_visible(cx, info.can_allow_once);
        let once_blurb = if info.can_allow_once { " Allow Once applies only to this request." } else { "" };
        let blurb = format!(
            "{} Choose where and for how long to allow it.{once_blurb} Room and space protection rules always take priority.",
            info.perm.blurb(),
        );
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

#[derive(Script, ScriptHook, Widget)]
pub struct PermissionScopeEditor {
    #[deref] view: View,
    #[rust] targets: Vec<(String, String, bool)>,
    #[rust] filtered: Vec<usize>,
    #[rust] rooms: BTreeSet<String>,
    #[rust] spaces: BTreeSet<String>,
    #[rust] all_rooms: bool,
    #[rust] origin_room: Option<String>,
    #[rust] choose_origin: bool,
    #[rust] session_rooms: Vec<String>,
    #[rust] session_origin_index: usize,
    #[rust] duration: usize,
    #[rust] network: bool,
    #[rust] network_kind: usize,
    #[rust] network_value: String,
}

impl Widget for PermissionScopeEditor {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        let Event::Actions(actions) = event else { return };
        if let Some(index) = self.view.drop_down(cx, ids!(scope_choice)).changed(actions) {
            self.all_rooms = index == 1;
            self.view.widget(cx, ids!(selected_targets)).set_visible(cx, !self.all_rooms);
        }
        if let Some(index) = self.view.drop_down(cx, ids!(duration_choice)).changed(actions) {
            self.duration = index;
            self.view.widget(cx, ids!(session_origin_section)).set_visible(cx, self.choose_origin && index == 0);
        }
        if let Some(index) = self.view.drop_down(cx, ids!(session_origin)).changed(actions) {
            self.session_origin_index = index;
        }
        if let Some(index) = self.view.drop_down(cx, ids!(network_choice)).changed(actions) {
            self.network_kind = index;
            self.view.widget(cx, ids!(network_value)).set_visible(cx, index != 4);
            self.update_network_summary(cx);
        }
        if let Some(value) = self.view.text_input(cx, ids!(network_value)).changed(actions) {
            self.network_value = value;
            self.update_network_summary(cx);
        }
        if let Some(filter) = self.view.text_input(cx, ids!(target_filter)).changed(actions) {
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
                let (id, _, is_space) = &self.targets[target_index];
                let selected = if *is_space { &mut self.spaces } else { &mut self.rooms };
                if checked { selected.insert(id.clone()); } else { selected.remove(id); }
            }
        }
        self.update_summary(cx);
        self.view.redraw(cx);
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
    fn update_summary(&self, cx: &mut Cx) {
        let text = if self.targets.is_empty() {
            "No rooms or spaces are loaded yet.".to_string()
        } else {
            format!("{} rooms and {} spaces selected. Spaces include their nested rooms.", self.rooms.len(), self.spaces.len())
        };
        self.view.label(cx, ids!(target_summary)).set_text(cx, &text);
    }

    fn network_scope(&self) -> Result<Option<NetworkScope>, String> {
        if !self.network { return Ok(None) }
        if self.network_kind == 4 { return Ok(Some(NetworkScope::AllHosts)) }
        let kind = match self.network_kind {
            0 => NetworkScopeKind::ExactUrl,
            1 => NetworkScopeKind::Origin,
            2 => NetworkScopeKind::Host,
            3 => NetworkScopeKind::Domain,
            _ => NetworkScopeKind::AllHosts,
        };
        NetworkScope::from_url(self.network_value.trim(), kind).map(Some)
    }

    fn update_network_summary(&self, cx: &mut Cx) {
        let text = match self.network_scope() {
            Ok(Some(network)) => network_scope_label(&network),
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
        inner.all_rooms = room_id.is_none() && !policy;
        inner.view.drop_down(cx, ids!(scope_choice)).set_selected_item(cx, usize::from(inner.all_rooms));
        inner.view.widget(cx, ids!(scope_choice)).set_visible(cx, !policy);
        inner.view.widget(cx, ids!(selected_targets)).set_visible(cx, !inner.all_rooms);
        inner.view.text_input(cx, ids!(target_filter)).set_text(cx, "");
        inner.view.portal_list(cx, ids!(targets)).set_first_id_and_scroll(0, 0.0);
        let mut durations = Vec::new();
        if let Some(origin) = origin_room {
            let name = inner.targets.iter().find(|(id, _, _)| id == origin).map(|(_, name, _)| name.as_str()).unwrap_or(origin);
            durations.push(format!("Until {name} closes"));
        }
        durations.extend(["Until Robrix closes".to_string(), "Always".to_string()]);
        inner.view.drop_down(cx, ids!(duration_choice)).set_labels(cx, durations);
        inner.view.drop_down(cx, ids!(duration_choice)).set_selected_item(cx, 0);
        inner.view.widget(cx, ids!(duration_section)).set_visible(cx, !policy);
        inner.view.widget(cx, ids!(network_section)).set_visible(cx, network);
        inner.view.widget(cx, ids!(network_value)).set_visible(cx, true);
        inner.view.drop_down(cx, ids!(network_choice)).set_selected_item(cx, 0);
        inner.view.text_input(cx, ids!(network_value)).set_text(cx, network_url.unwrap_or(""));
        inner.update_summary(cx);
        inner.update_network_summary(cx);
        inner.view.redraw(cx);
    }

    pub(crate) fn selection(&self) -> Result<PermissionSelection, String> {
        let Some(inner) = self.borrow() else { return Err("Permission editor is unavailable.".into()) };
        let scope = if inner.all_rooms { RoomScope::AllRooms } else {
            if inner.rooms.is_empty() && inner.spaces.is_empty() {
                return Err("Select at least one room or space.".into());
            }
            RoomScope::Selection { rooms: inner.rooms.iter().cloned().collect(), spaces: inner.spaces.iter().cloned().collect() }
        };
        let duration = match (inner.origin_room.is_some() || inner.choose_origin, inner.duration) {
            (true, 0) => GrantDuration::RoomSession,
            (true, 1) | (false, 0) => GrantDuration::RobrixSession,
            _ => GrantDuration::Always,
        };
        let origin_room = if duration == GrantDuration::RoomSession && inner.choose_origin {
            Some(inner.session_rooms.get(inner.session_origin_index).cloned().ok_or("Choose a source room for the room session.")?)
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
        inner.view.drop_down(cx, ids!(session_origin)).set_labels(cx, rooms.into_iter().map(|(_, name, _)| name).collect());
        inner.view.drop_down(cx, ids!(session_origin)).set_selected_item(cx, 0);
        inner.view.drop_down(cx, ids!(duration_choice)).set_labels(cx, vec![
            "Until the source room closes".into(), "Until Robrix closes".into(), "Always".into(),
        ]);
        inner.duration = 1;
        inner.view.drop_down(cx, ids!(duration_choice)).set_selected_item(cx, 1);
    }

    pub(crate) fn set_scope(&self, cx: &mut Cx, scope: &RoomScope) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.all_rooms = matches!(scope, RoomScope::AllRooms);
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
        inner.view.drop_down(cx, ids!(scope_choice)).set_selected_item(cx, usize::from(inner.all_rooms));
        inner.view.widget(cx, ids!(selected_targets)).set_visible(cx, !inner.all_rooms);
        inner.update_summary(cx);
        inner.view.redraw(cx);
    }
}

pub(crate) fn network_scope_label(scope: &NetworkScope) -> String {
    match scope {
        NetworkScope::ExactUrl(url) => format!("URL: {url}"),
        NetworkScope::Origin(origin) => format!("Site: {origin}"),
        NetworkScope::Host(host) => format!("Host: {host} (any HTTP/S port)"),
        NetworkScope::Domain(domain) => format!("Domain: {domain} and subdomains (any HTTP/S port)"),
        NetworkScope::AllHosts => "All internet sites".into(),
    }
}

pub(crate) fn duration_label(duration: GrantDuration) -> &'static str {
    match duration {
        GrantDuration::RoomSession => "Until the source room closes",
        GrantDuration::RobrixSession => "Until Robrix closes",
        GrantDuration::Always => "Always",
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
}
