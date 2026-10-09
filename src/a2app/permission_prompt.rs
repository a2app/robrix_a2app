//! One permission decision for related mini-app actions and their approval duration.
//!
//! Shown one at a time; the queue lives in [`crate::a2app::runtime`].

use makepad_widgets::*;
use std::collections::BTreeSet;
use a2app_core::permissions::{GrantDuration, NetworkScope, NetworkScopeKind, Permission, RoomScope};
use crate::home::rooms_list::RoomsListRef;
use crate::shared::collapsible_header::CollapsibleHeaderWidgetExt;
use super::permission_choices::*;
use super::permission_message_preview::PermissionMessageContentWidgetRefExt;

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    mod.widgets.PermissionOptionLabel = Label {
        width: Fill, height: Fit, padding: 0, margin: 0
        flow: Flow.Right{wrap: true}
        draw_text +: { text_style: SETTINGS_REGULAR_TEXT_STYLE {}, color: (MESSAGE_TEXT_COLOR) }
    }
    // Use container padding once, rather than Label's padding, to line up
    // every paragraph and heading with the dropdown's selected text.
    mod.widgets.PermissionPromptTextBlock = View {
        width: Fill, height: Fit, flow: Down
        padding: Inset{left: (PERMISSION_TEXT_INSET), right: (PERMISSION_TEXT_INSET)}
    }
    mod.widgets.PermissionPromptHeading = SettingsItemLabel {
        width: Fill, height: Fit, margin: 0, padding: 0
        flow: Flow.Right{wrap: true}
    }
    mod.widgets.PermissionDropDown = RobrixSettingsDropDown {
        width: Fill, margin: 0
        draw_text +: { max_lines: 1, text_overflow: TextOverflow.Ellipsis }
    }
    mod.widgets.PermissionScopeEditor = set_type_default() do #(PermissionScopeEditor::register_widget(vm)) {
        width: Fill, height: Fit, flow: Down, spacing: 8
        scope_heading := SettingsItemLabel { text: "Rooms and spaces" }
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
                editor_targets := View {
                    width: Fill, height: Fit
                    padding: Inset{left: (PERMISSION_TEXT_INSET), right: (PERMISSION_TEXT_INSET)}
                    targets := PortalList {
                        width: Fill, height: 168, flow: Down
                        target := RobrixSettingsCheckBox {}
                    }
                }
                popup_targets_section := View {
                    visible: false, width: Fill, height: Fit, flow: Down
                    padding: Inset{left: (PERMISSION_TEXT_INSET), right: (PERMISSION_TEXT_INSET)}
                    popup_targets := FlatList {
                        width: Fill, height: Fit, flow: Down
                        scroll_bars: ScrollBars { show_scroll_x: false, show_scroll_y: false }
                        target := RobrixSettingsCheckBox {}
                    }
                }
                target_empty_row := mod.widgets.PermissionPromptTextBlock {
                    visible: false
                    target_empty := mod.widgets.PermissionOptionLabel { visible: false }
                }
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
                    text: "Connect to the websites selected above. Sending private room or account data still needs your sharing permission."
                }
            }
        }
    }

    mod.widgets.MiniAppPermissionPrompt = set_type_default() do #(MiniAppPermissionPrompt::register_widget(vm)) {
        ..mod.widgets.SmallModal
        width: Fill { max: 560 }, padding: 0
        scroll_bars: ScrollBars { show_scroll_x: false, show_scroll_y: false }
        // Keep the rounded frame fixed while every part of the dialog scrolls.
        // The 40px viewport inset accounts for SmallModal's outer margins.
        // Inner margins keep the controls clear of the frame even at the bottom.
        dialog_scroll := ScrollYView {
            width: Fill, height: Fit { max: "calc(100vh - 40px)" }
            margin: 20, padding: Inset{right: 12}, flow: Down
            View {
                width: Fill, height: Fit, new_batch: true
                prompt_title := ModalTitle { margin: Inset{bottom: 14} }
            }
            prompt_content := View {
                width: Fill, height: Fit, flow: Down, spacing: 12
                mod.widgets.PermissionPromptTextBlock {
                    spacing: 12
                    prompt_blurb := ModalBody { margin: 0, padding: 0 }
                    prompt_message := PermissionMessageContent { visible: false }
                    prompt_write_warning := mod.widgets.PermissionOptionLabel { visible: false }
                    prompt_context := mod.widgets.PermissionOptionLabel {}
                    prompt_reason := mod.widgets.PermissionOptionLabel { visible: false }
                }
                prompt_tool := RoundedView {
                    width: Fill, height: Fit, visible: false, flow: Down, padding: 12, spacing: 8
                    draw_bg +: { color: (COLOR_BG_PREVIEW), border_radius: 4.0 }
                    mod.widgets.PermissionPromptHeading { text: "Tool provided by the mini-app" }
                    tool_label := mod.widgets.PermissionOptionLabel {}
                }
                group_section := View {
                    visible: false, width: Fill, height: Fit, flow: Down, spacing: 8
                    group_requests := FlatList {
                        width: Fill, height: Fit, flow: Down, spacing: 12
                        scroll_bars: ScrollBars { show_scroll_x: false, show_scroll_y: false }
                        Request := mod.widgets.PermissionPromptTextBlock {
                            spacing: 4
                            request_action := mod.widgets.PermissionPromptHeading {}
                            request_message := PermissionMessageContent { visible: false }
                            request_write_warning := mod.widgets.PermissionOptionLabel { visible: false }
                            request_context := mod.widgets.PermissionOptionLabel {}
                            request_reason := mod.widgets.PermissionOptionLabel {}
                            request_tool := mod.widgets.PermissionOptionLabel {}
                        }
                    }
                    group_choices_toggle := RobrixSettingsCollapsibleHeader { title: "Choose permissions individually" }
                    group_choices_body := View {
                        visible: false, width: Fill, height: Fit, flow: Down, spacing: 4
                        mod.widgets.PermissionPromptTextBlock {
                            mod.widgets.PermissionOptionLabel { text: "Leave checked the permissions you want to approve. Unchecked requests will be denied." }
                        }
                        group_choices := FlatList {
                            width: Fill, height: Fit, flow: Down
                            padding: Inset{left: (PERMISSION_TEXT_INSET), right: (PERMISSION_TEXT_INSET)}
                            scroll_bars: ScrollBars { show_scroll_x: false, show_scroll_y: false }
                            Request := RobrixSettingsCheckBox {
                                align: Align{x: 0.0, y: 0.0}
                                label_align: Align{x: 0.0, y: 0.0}
                                draw_bg +: {
                                    // Reuse the checkbox material with its mark centered on
                                    // the first line's 40px row, even when the label wraps.
                                    fragment: fn() {
                                        self.pos.y = self.pos.y + 0.5 - 20.0 / self.rect_size.y
                                        self.fb0 = depth_clip(self.world, self.pixel(), self.depth_clip)
                                    }
                                }
                            }
                        }
                    }
                    mod.widgets.PermissionPromptTextBlock {
                        group_selection_summary := mod.widgets.PermissionOptionLabel {}
                    }
                }
                flow_section := View {
                    visible: false, width: Fill, height: Fit, flow: Down, spacing: 10
                    flow_destination_row := mod.widgets.PermissionPromptTextBlock {
                        flow_destination := mod.widgets.PermissionOptionLabel {}
                    }
                    flow_payload_toggle := RobrixSettingsCollapsibleHeader {}
                    flow_payload_body := View {
                        visible: false, width: Fill, height: Fit, flow: Down, new_batch: true
                        padding: Inset{left: 36, top: 4, bottom: 4}
                        flow_payload := mod.widgets.PermissionOptionLabel { visible: false }
                    }
                }
                spatial_section := View {
                    visible: false, width: Fill, height: Fit, flow: Down, spacing: 8
                    spatial_choice_section := View {
                        width: Fill, height: Fit, flow: Down, spacing: 8
                        mod.widgets.PermissionPromptTextBlock {
                            mod.widgets.PermissionPromptHeading { text: "Approve in:" }
                        }
                        spatial_scope := PermissionDurationDropDown {
                            labels: ["Current room(s)", "All rooms", "Rooms in a space", "Selected rooms and spaces"]
                        }
                    }
                    spatial_picker := mod.widgets.PermissionScopeEditor { visible: false }
                    mod.widgets.PermissionPromptTextBlock {
                        spatial_summary := mod.widgets.PermissionOptionLabel {}
                    }
                }
            }
            approval_duration_section := View {
                width: Fill, height: Fit, flow: Down, spacing: 8, margin: Inset{top: 12}
                mod.widgets.PermissionPromptTextBlock {
                    mod.widgets.PermissionPromptHeading { text: "Approve duration:" }
                }
                approval_duration := PermissionDurationDropDown { labels: ["Until you quit Robrix", "Forever"] }
                mod.widgets.PermissionPromptTextBlock {
                    RoundedView {
                        width: Fill, height: Fit, flow: Down, padding: 12
                        draw_bg +: {
                            color: (COLOR_BG_LAVENDER), border_radius: 6.0
                            border_color: (COLOR_BG_LAVENDER_DOWN), border_size: 1.0
                        }
                        duration_summary := mod.widgets.PermissionOptionLabel {
                            draw_text +: { color: (COLOR_TEXT) }
                        }
                    }
                }
            }
            mod.widgets.PermissionPromptTextBlock {
                margin: Inset{top: 8}
                permission_help := mod.widgets.PermissionOptionLabel {}
            }
            ModalButtonsRow {
                spacing: 8, padding: Inset{top: 16, bottom: 20}
                not_now_button := RobrixNegativeIconButton {
                    padding: 12, align: Align{x: 0.5, y: 0.5}, text: "Deny"
                    draw_icon.svg: (ICON_FORBIDDEN)
                    icon_walk: Walk{width: 16, height: 16, margin: Inset{left: -2, right: -1}}
                }
                allow_button := RobrixPositiveIconButton {
                    padding: 12, align: Align{x: 0.5, y: 0.5}, text: "Approve"
                    draw_icon.svg: (ICON_CHECKMARK)
                    icon_walk: Walk{width: 16, height: 16, margin: Inset{left: -2, right: -1}}
                }
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

/// The captured message content, using the same formatting as its send path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionMessagePreview {
    pub body: String,
    pub formatted_html: Option<String>,
}

/// What the prompt modal needs to display one request.
#[derive(Clone, Debug)]
pub struct PromptInfo {
    /// The host-issued id of the pending permission request.
    pub prompt_id: u64,
    pub app_name: String,
    pub app_icon: String,
    pub perm: Permission,
    /// The app author's own `why-<perm>` reason, if declared.
    pub reason: Option<String>,
    /// The specific ability that triggered the ask (its catalog title), when
    /// a parked request identifies one.
    pub capability: Option<String>,
    pub message_preview: Option<PermissionMessagePreview>,
    /// Whether the asker is a room's AI agent (vs an installed mini-app):
    /// changes the wording of the blurb and reason lines.
    pub agent: bool,
    /// For an `mcp-tools` prompt: the app-authored tool text under review.
    pub tool: Option<ToolPreview>,
    /// The actual target of this request, which can differ from its source room.
    pub room_id: Option<String>,
    /// The captured rooms or collection, present only for spatial requests.
    pub scope: Option<RoomScope>,
    /// Required request targets paired with their trusted ancestor spaces.
    pub scope_targets: Vec<(String, Vec<String>)>,
    /// Closing this room expires a RoomSession grant.
    pub origin_room_id: Option<String>,
    pub network_url: Option<String>,
    /// A concrete request can be replayed once; subscriptions need a duration.
    pub can_allow_once: bool,
    /// Collection reads cover the user's rooms and spaces, subject to room protection.
    pub collection: bool,
    /// This approval explicitly enables the global room-write switch.
    pub enable_writes: bool,
}

/// A complete host-captured action and its private-data destination.
#[derive(Clone, Debug)]
pub struct FlowPromptInfo {
    /// The host-issued id of the pending permission request.
    pub prompt_id: u64,
    pub app_name: String,
    pub app_icon: String,
    /// Whether this action belongs to a room's AI agent.
    pub agent: bool,
    pub action: String,
    pub destination: String,
    pub sources: Vec<String>,
    pub payload: String,
    pub message_preview: Option<PermissionMessagePreview>,
    pub allow_once: bool,
    /// Host notice displayed outside the captured message content.
    pub write_warning: Option<String>,
    /// The host can preserve an approval beyond the current request.
    pub allow_lasting: bool,
    /// Room-relevant requests offer this captured scope as their default.
    pub scope: Option<RoomScope>,
    /// Required request targets paired with their trusted ancestor spaces.
    pub scope_targets: Vec<(String, Vec<String>)>,
    /// The current room for a collection request that defaults to all rooms.
    pub room_id: Option<String>,
    /// The initiating room whose closure expires approval, independent of target scope.
    pub origin_room_id: Option<String>,
}

/// The user's answer, carried with the request id in a `PermissionPromptResponse`.
#[derive(Clone, Debug, Default)]
pub enum PermissionPromptAction {
    AllowScoped { scope: RoomScope, duration: GrantDuration, network: Option<NetworkScope> },
    /// Authorizes only the single parked request.
    AllowOnce,
    AllowFlowOnce,
    AllowFlowSession,
    AllowFlow { duration: GrantDuration },
    AllowFlowScoped { scope: RoomScope, duration: GrantDuration },
    Deny,
    /// Nothing persists; this (app, permission) stops asking for the session.
    NotNow,
    #[default]
    None,
}

/// Binds an answer to the request displayed when the user made the choice.
#[derive(Clone, Debug)]
pub struct PermissionPromptResponse {
    pub prompt_id: u64,
    pub answer: PermissionPromptAction,
}

/// The host-captured requests shown together in one permission decision.
#[derive(Clone, Debug)]
pub enum PermissionPromptInfo {
    Ordinary(PromptInfo),
    Flow(FlowPromptInfo),
}

#[derive(Clone, Debug)]
pub struct PermissionPromptGroupInfo {
    pub group_id: u64,
    pub requests: Vec<PermissionPromptInfo>,
}

/// Every displayed request receives exactly one answer, including unchecked ones.
#[derive(Clone, Debug)]
pub struct PermissionPromptGroupResponse {
    pub group_id: u64,
    pub responses: Vec<PermissionPromptResponse>,
}

impl PermissionPromptInfo {
    pub fn prompt_id(&self) -> u64 {
        match self { Self::Ordinary(info) => info.prompt_id, Self::Flow(info) => info.prompt_id }
    }

    fn identity(&self) -> (&str, &str) {
        match self {
            Self::Ordinary(info) => (&info.app_name, &info.app_icon),
            Self::Flow(info) => (&info.app_name, &info.app_icon),
        }
    }

    fn action(&self) -> String {
        match self {
            Self::Ordinary(info) => ordinary_action(info),
            Self::Flow(info) => info.action.clone(),
        }
    }

    fn message_preview(&self) -> Option<&PermissionMessagePreview> {
        match self {
            Self::Ordinary(info) => info.message_preview.as_ref(),
            Self::Flow(info) => info.message_preview.as_ref(),
        }
    }

    fn write_warning(&self) -> &str {
        match self {
            Self::Ordinary(info) if info.enable_writes && info.message_preview.is_some() => ROOM_WRITE_WARNING,
            Self::Flow(info) => info.write_warning.as_deref().unwrap_or(""),
            _ => "",
        }
    }

    fn context(&self, cx: &mut Cx) -> String {
        match self {
            Self::Ordinary(info) => ordinary_context(cx, info),
            Self::Flow(info) => {
                if info.destination.starts_with("Your Matrix server: ") || info.destination == "Robrix" { String::new() }
                else { flow_target(info) }
            }
        }
    }

    fn scope(&self) -> Option<&RoomScope> {
        match self { Self::Ordinary(info) => info.scope.as_ref(), Self::Flow(info) => info.scope.as_ref() }
    }

    fn scope_targets(&self) -> &[(String, Vec<String>)] {
        match self { Self::Ordinary(info) => &info.scope_targets, Self::Flow(info) => &info.scope_targets }
    }

    fn room_id(&self) -> Option<&str> {
        match self { Self::Ordinary(info) => info.room_id.as_deref(), Self::Flow(info) => info.room_id.as_deref() }
    }

    fn allows_once(&self) -> bool {
        match self {
            Self::Ordinary(info) => info.can_allow_once && !info.enable_writes,
            Self::Flow(info) => info.allow_once,
        }
    }

    fn allows_lasting(&self) -> bool {
        match self { Self::Ordinary(_) => true, Self::Flow(info) => info.allow_lasting }
    }

    fn origin_room_id(&self) -> Option<&str> {
        match self {
            Self::Ordinary(info) => info.origin_room_id.as_deref(),
            Self::Flow(info) => info.origin_room_id.as_deref(),
        }.filter(|room| !room.is_empty())
    }

    fn valid(&self) -> bool {
        match self {
            Self::Ordinary(info) => info.perm != Permission::Network || info.network_url.as_deref()
                .and_then(|url| NetworkScope::from_url(url, NetworkScopeKind::Origin).ok()).is_some(),
            Self::Flow(_) => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApprovalDuration { Choose, Once, RoomSession, Session, Forever }

impl ApprovalDuration {
    fn grant(self) -> Option<GrantDuration> {
        match self {
            Self::RoomSession => Some(GrantDuration::RoomSession),
            Self::Session => Some(GrantDuration::RobrixSession),
            Self::Forever => Some(GrantDuration::Always),
            Self::Choose | Self::Once => None,
        }
    }
}

/// A grouped room lifetime must have one shared, host-captured closing boundary.
fn shared_origin_room(requests: &[&PermissionPromptInfo]) -> Option<String> {
    let origin = requests.first()?.origin_room_id()?;
    requests.iter().all(|request| request.allows_lasting() && request.origin_room_id() == Some(origin))
        .then(|| origin.to_owned())
}

#[derive(Script, ScriptHook, Widget)]
pub struct MiniAppPermissionPrompt {
    #[deref] view: View,
    #[rust] prompt_id: u64,
    #[rust] group_id: Option<u64>,
    #[rust] group_requests: Vec<PermissionPromptInfo>,
    #[rust] group_selected: Vec<bool>,
    #[rust] group_choices_expanded: bool,
    #[rust] flow_mode: bool,
    #[rust] flow_payload_expanded: bool,
    #[rust] durations: Vec<ApprovalDuration>,
    #[rust] duration: usize,
    #[rust] duration_origin_room_id: Option<String>,
    #[rust] scope: Option<RoomScope>,
    #[rust] captured_scope: Option<RoomScope>,
    #[rust] current_scope: Option<RoomScope>,
    #[rust] scope_targets: Vec<(String, Vec<String>)>,
    #[rust] spatial_modes: Vec<usize>,
    #[rust] spatial_mode: usize,
    #[rust] spatial_choice_changed: bool,
    #[rust] spatial_available: bool,
    #[rust] scope_valid: bool,
    #[rust] space_membership_ready: bool,
    #[rust] network: Option<NetworkScope>,
    #[rust] request_valid: bool,
}

impl Widget for MiniAppPermissionPrompt {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        let Event::Actions(actions) = event else { return };
        if self.group_id.is_some() {
            let choices = self.view.collapsible_header(cx, ids!(group_choices_toggle));
            if let Some(expanded) = choices.expansion_changed(actions) {
                self.group_choices_expanded = expanded;
                self.view.widget(cx, ids!(group_choices_body)).set_visible(cx, expanded);
                self.view.redraw(cx);
            }
            let mut changed = false;
            for (id, row) in self.view.flat_list(cx, ids!(group_choices)).items_with_actions(actions) {
                let Some(index) = id.0.checked_sub(1).and_then(|index| usize::try_from(index).ok()) else { continue };
                if self.group_choices_expanded && let Some(checked) = row.as_check_box().changed(actions)
                    && let Some(selected) = self.group_selected.get_mut(index)
                {
                    *selected = checked;
                    changed = true;
                }
            }
            if changed { self.configure_group_selection(cx, false); }
        }
        if let Some(index) = self.view.drop_down2(cx, ids!(approval_duration)).selected(actions) {
            if index < self.durations.len() {
                self.duration = index;
            }
            self.refresh_duration(cx);
        }
        if self.spatial_available && let Some(index) = self.view.drop_down2(cx, ids!(spatial_scope)).selected(actions) {
            if let Some(mode) = self.spatial_modes.get(index).copied() { self.select_spatial_scope(cx, mode); }
            self.refresh_duration(cx);
        }
        let picker = self.view.permission_scope_editor(cx, ids!(spatial_picker));
        if self.spatial_available && self.spatial_mode >= 2
            && !matches!(self.durations.get(self.duration), Some(ApprovalDuration::Once))
            && matches!(actions.find_widget_action(picker.widget_uid()).cast(), PermissionScopeEditorAction::Changed)
        {
            self.spatial_choice_changed = true;
            self.scope = picker.selection().ok().map(|selection| selection.scope);
            self.refresh_duration(cx);
        }
        let details = self.view.collapsible_header(cx, ids!(flow_payload_toggle));
        if let Some(expanded) = details.expansion_changed(actions) {
            self.flow_payload_expanded = expanded;
            self.view.widget(cx, ids!(flow_payload_body)).set_visible(cx, expanded);
            self.view.widget(cx, ids!(flow_payload)).set_visible(cx, self.flow_payload_expanded);
            self.view.redraw(cx);
        }
        if self.view.button(cx, ids!(not_now_button)).clicked(actions) {
            self.answer(cx, PermissionPromptAction::NotNow);
        } else if self.request_valid && self.scope_valid && self.view.button(cx, ids!(allow_button)).clicked(actions) {
            let Some(duration) = self.durations.get(self.duration).copied() else { return };
            let answer = match (self.flow_mode, duration) {
                (_, ApprovalDuration::Choose) => return,
                (true, ApprovalDuration::Once) => PermissionPromptAction::AllowFlowOnce,
                (true, duration) if self.spatial_available => PermissionPromptAction::AllowFlowScoped {
                    scope: self.scope.clone().unwrap_or(RoomScope::AllRooms),
                    duration: duration.grant().unwrap(),
                },
                (true, ApprovalDuration::RoomSession) => PermissionPromptAction::AllowFlow { duration: GrantDuration::RoomSession },
                (true, ApprovalDuration::Session) => PermissionPromptAction::AllowFlow { duration: GrantDuration::RobrixSession },
                (true, ApprovalDuration::Forever) => PermissionPromptAction::AllowFlow { duration: GrantDuration::Always },
                (false, ApprovalDuration::Once) => PermissionPromptAction::AllowOnce,
                (false, duration) => PermissionPromptAction::AllowScoped {
                    scope: self.scope.clone().unwrap_or(RoomScope::AllRooms),
                    duration: duration.grant().unwrap(),
                    network: self.network.clone(),
                },
            };
            self.answer(cx, answer);
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        let requests = self.view.flat_list(cx, ids!(group_requests)).widget_uid();
        let choices = self.view.flat_list(cx, ids!(group_choices)).widget_uid();
        while let Some(widget) = self.view.draw_walk(cx, scope, walk).step() {
            let summary = widget.widget_uid() == requests;
            if (summary || widget.widget_uid() == choices) && let Some(mut list) = widget.as_flat_list().borrow_mut() {
                for (index, request) in self.group_requests.iter().enumerate() {
                    let Some(row) = list.item(cx, LiveId(index as u64 + 1), id!(Request)) else { continue };
                    if summary {
                        let action = request.action();
                        let action = if self.group_selected.get(index).copied().unwrap_or(false) { action }
                            else { format!("Not selected: {action}") };
                        row.label(cx, ids!(request_action)).set_text(cx, &review_text(&action));
                        row.widget(cx, ids!(request_message)).as_permission_message_content().set_message(cx, request.message_preview());
                        let warning = request.write_warning();
                        row.label(cx, ids!(request_write_warning)).set_text(cx, warning);
                        row.widget(cx, ids!(request_write_warning)).set_visible(cx, !warning.is_empty());
                        let context = request.context(cx);
                        row.label(cx, ids!(request_context)).set_text(cx, &review_text(&context));
                        row.widget(cx, ids!(request_context)).set_visible(cx, !context.is_empty());
                        let reason = match request { PermissionPromptInfo::Ordinary(info) => info.reason.as_deref().unwrap_or(""), _ => "" };
                        row.label(cx, ids!(request_reason)).set_text(cx, &review_text(reason));
                        row.widget(cx, ids!(request_reason)).set_visible(cx, !reason.is_empty());
                        let tool = match request { PermissionPromptInfo::Ordinary(info) => info.tool.as_ref().map(tool_text), _ => None };
                        row.label(cx, ids!(request_tool)).set_text(cx, &review_text(tool.as_deref().unwrap_or("")));
                        row.widget(cx, ids!(request_tool)).set_visible(cx, tool.is_some());
                    } else {
                        row.set_text(cx, &review_text(&request.action()));
                        row.as_check_box().set_active(cx, self.group_selected.get(index).copied().unwrap_or(false), Animate::No);
                    }
                    row.draw_all(cx, scope);
                }
                list.items.retain_visible();
            }
        }
        DrawStep::done()
    }
}

impl MiniAppPermissionPrompt {
    fn answer(&self, cx: &mut Cx, answer: PermissionPromptAction) {
        if let Some(group_id) = self.group_id {
            let deny = matches!(answer, PermissionPromptAction::NotNow | PermissionPromptAction::Deny);
            let duration = self.durations.get(self.duration).copied();
            if !deny && matches!(duration, None | Some(ApprovalDuration::Choose)) { return }
            let responses = self.group_requests.iter().enumerate().map(|(index, request)| {
                let answer = if deny || !self.group_selected.get(index).copied().unwrap_or(false) { PermissionPromptAction::NotNow }
                else { self.group_answer(request, duration.unwrap()) };
                PermissionPromptResponse { prompt_id: request.prompt_id(), answer }
            }).collect();
            cx.action(PermissionPromptGroupResponse { group_id, responses });
            return;
        }
        cx.action(PermissionPromptResponse { prompt_id: self.prompt_id, answer });
    }

    fn group_answer(&self, request: &PermissionPromptInfo, duration: ApprovalDuration) -> PermissionPromptAction {
        let lasting = duration.grant().unwrap_or(GrantDuration::RobrixSession);
        match request {
            PermissionPromptInfo::Flow(_) if duration == ApprovalDuration::Once => PermissionPromptAction::AllowFlowOnce,
            PermissionPromptInfo::Flow(info) if info.scope.is_some() => PermissionPromptAction::AllowFlowScoped {
                scope: self.scope.clone().unwrap_or(RoomScope::AllRooms), duration: lasting,
            },
            PermissionPromptInfo::Flow(_) => PermissionPromptAction::AllowFlow { duration: lasting },
            PermissionPromptInfo::Ordinary(_) if duration == ApprovalDuration::Once => PermissionPromptAction::AllowOnce,
            PermissionPromptInfo::Ordinary(info) => PermissionPromptAction::AllowScoped {
                scope: if info.scope.is_some() { self.scope.clone().unwrap_or(RoomScope::AllRooms) }
                    else { info.room_id.as_deref().map(RoomScope::room).unwrap_or(RoomScope::AllRooms) },
                duration: lasting,
                network: info.network_url.as_deref().and_then(|url| NetworkScope::from_url(url, NetworkScopeKind::Origin).ok()),
            },
        }
    }

    fn configure_group_selection(&mut self, cx: &mut Cx, reset: bool) {
        let old_duration = (!reset).then(|| self.durations.get(self.duration).copied()).flatten();
        let old_origin_room = self.duration_origin_room_id.clone();
        let old_scope = self.scope.clone();
        let old_mode = self.spatial_mode;
        let old_spatial = self.spatial_available;
        let selected = self.group_requests.iter().enumerate().filter(|(index, _)| self.group_selected.get(*index).copied().unwrap_or(false))
            .map(|(_, request)| request).collect::<Vec<_>>();
        let count = selected.len();
        let once = count > 0 && selected.iter().all(|request| request.allows_once());
        let lasting = count > 0 && selected.iter().all(|request| request.allows_lasting());
        let origin_room = shared_origin_room(&selected);
        self.request_valid = count > 0 && selected.iter().all(|request| request.valid());
        let mut rooms = BTreeSet::new();
        let mut spaces = BTreeSet::new();
        let mut targets = std::collections::BTreeMap::<String, BTreeSet<String>>::new();
        let mut spatial = false;
        let mut all_rooms = false;
        let current_room = selected.iter().find_map(|request| request.scope().and(request.room_id())).map(str::to_owned);
        for request in &selected {
            if let Some(scope) = request.scope() {
                spatial = true;
                match scope {
                    RoomScope::AllRooms => all_rooms = true,
                    RoomScope::Selection { rooms: selected_rooms, spaces: selected_spaces } => {
                        rooms.extend(selected_rooms.iter().cloned());
                        spaces.extend(selected_spaces.iter().cloned());
                    },
                }
                for (room, ancestors) in request.scope_targets() { targets.entry(room.clone()).or_default().extend(ancestors.iter().cloned()); }
            }
        }
        drop(selected);
        let captured = spatial.then(|| if all_rooms { RoomScope::AllRooms }
            else { RoomScope::Selection { rooms: rooms.into_iter().collect(), spaces: spaces.into_iter().collect() } });
        let targets = targets.into_iter().map(|(room, ancestors)| (room, ancestors.into_iter().collect())).collect::<Vec<_>>();
        self.configure_spatial_scope(cx, captured, &targets, current_room.as_deref());
        if !reset && self.spatial_choice_changed && old_spatial && self.spatial_available && old_mode > 0 && self.spatial_modes.contains(&old_mode) {
            self.scope = old_scope;
            self.spatial_mode = old_mode;
            if old_mode >= 2 && let Some(scope) = self.scope.as_ref() {
                self.view.permission_scope_editor(cx, ids!(spatial_picker)).configure_popup(cx, scope, old_mode == 2);
            }
        }
        self.configure_duration(cx, once, lasting, origin_room.as_deref());
        if !reset {
            // Losing a chosen duration must require a new choice, not broaden consent.
            if let Some(index) = old_duration.filter(|old| *old != ApprovalDuration::RoomSession || old_origin_room == self.duration_origin_room_id)
                .and_then(|old| self.durations.iter().position(|duration| *duration == old))
            {
                self.duration = index;
            } else {
                self.durations.insert(0, ApprovalDuration::Choose);
                self.duration = 0;
            }
        }
        let summary = if count > 0 && !once && !lasting { "These requests need different approval durations. Choose permissions individually to approve one at a time.".into() }
            else if count == self.group_requests.len() && count > 0 { "Approve allows all the requests above.".into() }
            else if count == 0 { "Select at least one permission to approve, or choose Deny.".into() }
            else { format!("Approve allows {count} of {} requests. Unchecked requests will be denied.", self.group_requests.len()) };
        self.view.label(cx, ids!(group_selection_summary)).set_text(cx, &summary);
        let details = self.group_requests.iter().enumerate().map(|(index, request)| {
            let mut text = format!("{}{}", if self.group_selected.get(index).copied().unwrap_or(false) { "" } else { "Not selected: " }, request.action());
            match request {
                PermissionPromptInfo::Flow(info) => text.push_str(&format!("\n{}\n\n{}", info.destination, info.payload)),
                PermissionPromptInfo::Ordinary(info) => {
                    if let Some(message) = &info.message_preview { text.push_str(&format!("\n\n{}", message.body)); }
                    let context = ordinary_context(cx, info);
                    if !context.is_empty() { text.push_str(&format!("\n{context}")); }
                    if let Some(tool) = info.tool.as_ref() { text.push_str(&format!("\n\n{}", tool_text(tool))); }
                },
            }
            text
        }).collect::<Vec<_>>().join("\n\n");
        self.view.label(cx, ids!(flow_payload)).set_text(cx, &review_text(&details));
        self.refresh_duration(cx);
        self.view.redraw(cx);
    }

    fn configure_duration(&mut self, cx: &mut Cx, once: bool, lasting: bool, origin_room: Option<&str>) {
        self.durations.clear();
        if once { self.durations.push(ApprovalDuration::Once); }
        self.duration_origin_room_id = origin_room.filter(|room| lasting && !room.is_empty()).map(str::to_owned);
        if self.duration_origin_room_id.is_some() { self.durations.push(ApprovalDuration::RoomSession); }
        if lasting { self.durations.extend([ApprovalDuration::Session, ApprovalDuration::Forever]); }
        self.duration = self.durations.iter().position(|duration| *duration == ApprovalDuration::Session).unwrap_or(0);
        self.request_valid &= !self.durations.is_empty();
        let dropdown = self.view.drop_down2(cx, ids!(approval_duration));
        dropdown.set_labels(cx, self.duration_labels());
        dropdown.set_selected_item(cx, self.duration);
        self.refresh_duration(cx);
    }

    fn configure_spatial_scope(&mut self, cx: &mut Cx, scope: Option<RoomScope>, targets: &[(String, Vec<String>)], room: Option<&str>) {
        self.captured_scope = scope.clone();
        self.current_scope = match scope.as_ref() {
            Some(RoomScope::Selection { .. }) => scope.clone(),
            Some(RoomScope::AllRooms) => room.map(|room| {
                let is_space = cx.has_global::<RoomsListRef>() && cx.get_global::<RoomsListRef>().permission_targets()
                    .iter().any(|(id, _, space)| id == room && *space);
                if is_space { RoomScope::Selection { rooms: Vec::new(), spaces: vec![room.into()] } }
                else { RoomScope::room(room) }
            }),
            None => None,
        };
        self.scope = scope;
        self.scope_targets = targets.to_vec();
        self.spatial_available = self.captured_scope.is_some();
        self.spatial_mode = usize::from(matches!(self.captured_scope, Some(RoomScope::AllRooms)));
        self.spatial_modes = if self.current_scope.is_some() { vec![0, 1, 2, 3] } else { vec![1, 2, 3] };
        let labels = self.spatial_modes.iter().map(|mode| match mode {
            0 => match self.current_scope.as_ref() {
                Some(RoomScope::Selection { rooms, spaces }) if rooms.len() == 1 && spaces.is_empty() => "Current room",
                Some(RoomScope::Selection { spaces, .. }) if !spaces.is_empty() => "Current rooms and spaces",
                _ => "Current rooms",
            },
            1 => "All rooms", 2 => "Rooms in a space", _ => "Selected rooms and spaces",
        }.to_string()).collect();
        let dropdown = self.view.drop_down2(cx, ids!(spatial_scope));
        dropdown.set_labels(cx, labels);
        dropdown.set_selected_item(cx, self.spatial_modes.iter().position(|mode| *mode == self.spatial_mode).unwrap_or(0));
        self.view.widget(cx, ids!(spatial_section)).set_visible(cx, self.spatial_available);
        self.view.widget(cx, ids!(spatial_picker)).set_visible(cx, false);
    }

    fn select_spatial_scope(&mut self, cx: &mut Cx, index: usize) {
        if index >= 4 || matches!(self.durations.get(self.duration), Some(ApprovalDuration::Once)) { return }
        self.spatial_choice_changed = true;
        self.spatial_mode = index;
        self.scope = match index {
            0 => self.current_scope.clone(),
            1 => Some(RoomScope::AllRooms),
            _ => {
                let scope = self.scope.clone().unwrap_or_else(|| self.captured_scope.clone().unwrap_or(RoomScope::AllRooms));
                let picker = self.view.permission_scope_editor(cx, ids!(spatial_picker));
                picker.configure_popup(cx, &scope, index == 2);
                picker.selection().ok().map(|selection| selection.scope)
            },
        };
        self.refresh_duration(cx);
        self.view.redraw(cx);
    }

    fn refresh_duration(&mut self, cx: &mut Cx) {
        let once = matches!(self.durations.get(self.duration), Some(ApprovalDuration::Once));
        self.view.drop_down2(cx, ids!(approval_duration)).set_labels(cx, self.duration_labels());
        self.view.drop_down2(cx, ids!(approval_duration)).set_selected_item(cx, self.duration);
        self.view.drop_down2(cx, ids!(spatial_scope)).set_selected_item(cx,
            self.spatial_modes.iter().position(|mode| *mode == self.spatial_mode).unwrap_or(0));
        let selected_scope = if once { self.captured_scope.as_ref() } else { self.scope.as_ref() };
        let waiting_for_space = self.spatial_available && !self.space_membership_ready
            && matches!(selected_scope, Some(RoomScope::Selection { spaces, .. }) if !spaces.is_empty());
        self.scope_valid = !waiting_for_space && (!self.spatial_available || once
            || self.scope.as_ref().is_some_and(|scope| scope_covers_targets(scope, &self.scope_targets)));
        self.view.widget(cx, ids!(spatial_choice_section)).set_visible(cx, !once);
        self.view.widget(cx, ids!(spatial_picker)).set_visible(cx, self.spatial_available && !once && self.spatial_mode >= 2);
        let spatial_summary = if waiting_for_space {
            if once { "Loading rooms used by this request.".into() }
            else { "Loading rooms in the selected space. You can also choose other rooms.".into() }
        } else if once {
            "One time approves only the rooms used by the request shown.".into()
        } else if !self.scope_valid {
            let instruction = match (self.spatial_mode == 2, self.scope_targets.is_empty()) {
                (true, true) => "Choose a space.",
                (true, false) => "Choose a space that includes every room used by this request.",
                (false, true) => "Select at least one room or space.",
                (false, false) => "Select every room used by this request.",
            };
            if self.durations.contains(&ApprovalDuration::Once) { format!("{instruction} Or choose One time for only this request.") }
            else { instruction.into() }
        } else if let Some(scope) = self.scope.as_ref() {
            popup_scope_summary(cx, scope)
        } else { String::new() };
        self.view.label(cx, ids!(spatial_summary)).set_text(cx, &spatial_summary);
        let room_summary = self.duration_origin_room_id.as_deref().map(|origin| {
            let name = if cx.has_global::<RoomsListRef>() {
                cx.get_global::<RoomsListRef>().permission_targets().into_iter()
                    .find(|(id, _, _)| id == origin).map(|(_, name, _)| name)
            } else { None }.unwrap_or_else(|| "the starting room".into());
            format!("Closing the last room tab or screen for {name} expires this approval, even if the mini-app stays open separately.")
        });
        let summary = match self.durations.get(self.duration) {
            Some(ApprovalDuration::Choose) => "Your previous duration does not apply to every selected permission. Choose a duration above.",
            Some(ApprovalDuration::Once) if self.durations.len() == 1 => "Approval applies only to the request shown.",
            Some(ApprovalDuration::Once) if self.group_id.is_some() => "Only the selected requests. Ask again next time.",
            Some(ApprovalDuration::Once) => "Only this request. Ask again next time.",
            Some(ApprovalDuration::RoomSession) => room_summary.as_deref().unwrap_or("Choose how long to allow this action."),
            Some(ApprovalDuration::Session) if self.group_id.is_some() => "Use these permissions without asking again until you quit Robrix.",
            Some(ApprovalDuration::Session) => "Repeat this action without asking again until you quit Robrix.",
            Some(ApprovalDuration::Forever) if self.group_id.is_some() => "Keep these approvals until you remove them in Mini Apps.",
            Some(ApprovalDuration::Forever) => "Keep this approval until you remove it in Mini Apps.",
            None => "Choose how long to allow this action.",
        };
        self.view.label(cx, ids!(duration_summary)).set_text(cx, summary);
        let duration_valid = matches!(self.durations.get(self.duration), Some(ApprovalDuration::Once | ApprovalDuration::RoomSession | ApprovalDuration::Session | ApprovalDuration::Forever));
        let has_duration_choices = self.durations.iter().any(|duration| *duration != ApprovalDuration::Choose);
        self.view.widget(cx, ids!(approval_duration_section)).set_visible(cx, has_duration_choices);
        let can_approve = self.request_valid && self.scope_valid && duration_valid;
        self.view.button(cx, ids!(allow_button)).set_enabled(cx, can_approve);
        self.view.widget(cx, ids!(allow_button)).set_disabled(cx, !can_approve);
    }

    fn duration_labels(&self) -> Vec<String> {
        self.durations.iter().map(|duration| match duration {
            ApprovalDuration::Choose => "Choose a duration…",
            ApprovalDuration::Once => "One time", ApprovalDuration::RoomSession => "Until this room closes",
            ApprovalDuration::Session => "Until you quit Robrix", ApprovalDuration::Forever => "Forever",
        }.into()).collect()
    }
}

impl MiniAppPermissionPromptRef {
    /// Shows one decision with its captured target and approval duration.
    pub fn show(&self, cx: &mut Cx, info: &PromptInfo) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.group_id = None;
        inner.group_requests.clear();
        inner.group_selected.clear();
        inner.spatial_choice_changed = false;
        inner.view.widget(cx, ids!(group_section)).set_visible(cx, false);
        inner.prompt_id = info.prompt_id;
        inner.flow_mode = false;
        inner.space_membership_ready = true;
        inner.flow_payload_expanded = false;
        inner.view.collapsible_header(cx, ids!(flow_payload_toggle)).set_expanded(cx, false, Animate::No);
        inner.view.widget(cx, ids!(flow_payload_body)).set_visible(cx, false);
        inner.scope = Some(info.scope.clone().unwrap_or_else(|| if info.collection { RoomScope::AllRooms } else {
            info.room_id.as_deref().map(RoomScope::room).unwrap_or(RoomScope::AllRooms)
        }));
        let captured_scope = inner.scope.clone();
        inner.configure_spatial_scope(cx, info.scope.clone(), &info.scope_targets, info.room_id.as_deref());
        if !inner.spatial_available { inner.scope = captured_scope; }
        inner.network = info.network_url.as_deref().and_then(|url| NetworkScope::from_url(url, NetworkScopeKind::Origin).ok());
        inner.request_valid = info.perm != Permission::Network || inner.network.is_some();
        inner.view.view(cx, ids!(dialog_scroll)).set_scroll_pos(cx, Vec2d::default());
        inner.view.widget(cx, ids!(flow_section)).set_visible(cx, info.message_preview.is_some());
        inner.view.widget(cx, ids!(flow_destination_row)).set_visible(cx, false);
        inner.view.widget(cx, ids!(flow_payload)).set_visible(cx, false);
        inner.view.label(cx, ids!(flow_payload)).set_text(cx, &review_text(info.message_preview.as_ref().map(|message| message.body.as_str()).unwrap_or("")));
        inner.view.label(cx, ids!(prompt_title)).set_text(cx, &review_text(&format!("{} {} needs permission", info.app_icon, info.app_name)));
        let blurb = ordinary_action(info);
        inner.view.label(cx, ids!(prompt_blurb)).set_text(cx, &review_text(&blurb));
        inner.view.widget(cx, ids!(prompt_message)).as_permission_message_content().set_message(cx, info.message_preview.as_ref());
        let warning = if info.enable_writes && info.message_preview.is_some() { ROOM_WRITE_WARNING } else { "" };
        inner.view.label(cx, ids!(prompt_write_warning)).set_text(cx, warning);
        inner.view.widget(cx, ids!(prompt_write_warning)).set_visible(cx, !warning.is_empty());
        let context = ordinary_context(cx, info);
        inner.view.label(cx, ids!(prompt_context)).set_text(cx, &review_text(&context));
        inner.view.widget(cx, ids!(prompt_context)).set_visible(cx, !context.is_empty());
        let reason = info.reason.as_deref().unwrap_or("");
        inner.view.label(cx, ids!(prompt_reason)).set_text(cx, &review_text(reason));
        inner.view.widget(cx, ids!(prompt_reason)).set_visible(cx, !reason.is_empty());
        match &info.tool {
            Some(tool) => {
                let text = tool_text(tool);
                inner.view.label(cx, ids!(tool_label)).set_text(cx, &review_text(&text));
                inner.view.widget(cx, ids!(prompt_tool)).set_visible(cx, true);
            }
            None => inner.view.widget(cx, ids!(prompt_tool)).set_visible(cx, false),
        }
        inner.view.label(cx, ids!(permission_help)).set_text(cx, &review_text(&permission_help(&info.app_name, info.agent, false)));
        inner.configure_duration(cx, info.can_allow_once && !info.enable_writes, true, info.origin_room_id.as_deref());
        inner.view.redraw(cx);
    }

    pub fn show_flow(&self, cx: &mut Cx, info: &FlowPromptInfo) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.group_id = None;
        inner.group_requests.clear();
        inner.group_selected.clear();
        inner.spatial_choice_changed = false;
        inner.view.widget(cx, ids!(group_section)).set_visible(cx, false);
        inner.prompt_id = info.prompt_id;
        inner.flow_mode = true;
        inner.space_membership_ready = true;
        inner.flow_payload_expanded = false;
        inner.view.collapsible_header(cx, ids!(flow_payload_toggle)).set_expanded(cx, false, Animate::No);
        inner.view.widget(cx, ids!(flow_payload_body)).set_visible(cx, false);
        inner.configure_spatial_scope(cx, info.scope.clone(), &info.scope_targets, info.room_id.as_deref());
        inner.network = None;
        inner.request_valid = true;
        inner.view.view(cx, ids!(dialog_scroll)).set_scroll_pos(cx, Vec2d::default());
        for id in [ids!(prompt_context), ids!(prompt_reason), ids!(prompt_tool), ids!(prompt_write_warning)] { inner.view.widget(cx, id).set_visible(cx, false); }
        inner.view.widget(cx, ids!(prompt_message)).as_permission_message_content().set_message(cx, info.message_preview.as_ref());
        let warning = info.write_warning.as_deref().unwrap_or("");
        inner.view.label(cx, ids!(prompt_write_warning)).set_text(cx, &review_text(warning));
        inner.view.widget(cx, ids!(prompt_write_warning)).set_visible(cx, !warning.is_empty());
        inner.view.widget(cx, ids!(flow_section)).set_visible(cx, true);
        inner.view.widget(cx, ids!(flow_payload)).set_visible(cx, false);
        inner.view.label(cx, ids!(prompt_title)).set_text(cx, &review_text(&format!("{} {} needs permission", info.app_icon, info.app_name)));
        inner.view.label(cx, ids!(prompt_blurb)).set_text(cx, &review_text(&info.action));
        let target = flow_target(info);
        let matrix_server = info.destination.starts_with("Your Matrix server: ");
        inner.view.label(cx, ids!(flow_destination)).set_text(cx, &review_text(&target));
        let show_destination = !matrix_server && !target.is_empty() && target != "Robrix";
        inner.view.widget(cx, ids!(flow_destination)).set_visible(cx, show_destination);
        inner.view.widget(cx, ids!(flow_destination_row)).set_visible(cx, show_destination);
        let details = if matrix_server { format!("{}\n\n{}", info.destination, info.payload) } else { info.payload.clone() };
        inner.view.label(cx, ids!(flow_payload)).set_text(cx, &review_text(&details));
        inner.view.label(cx, ids!(permission_help)).set_text(cx, &review_text(&permission_help(&info.app_name, info.agent, false)));
        inner.configure_duration(cx, info.allow_once, info.allow_lasting, info.origin_room_id.as_deref());
        inner.view.redraw(cx);
    }

    /// Shows related requests with one approval and optional individual choices.
    pub fn show_group(&self, cx: &mut Cx, info: &PermissionPromptGroupInfo) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.group_id = Some(info.group_id);
        inner.group_requests = info.requests.clone();
        inner.group_selected = vec![true; info.requests.len()];
        inner.group_choices_expanded = false;
        inner.spatial_choice_changed = false;
        let has_flow = info.requests.iter().any(|request| matches!(request, PermissionPromptInfo::Flow(_)));
        let has_details = has_flow || info.requests.iter().any(|request| request.message_preview().is_some());
        inner.flow_mode = has_flow;
        inner.flow_payload_expanded = false;
        inner.space_membership_ready = true;
        inner.network = None;
        inner.view.view(cx, ids!(dialog_scroll)).set_scroll_pos(cx, Vec2d::default());
        inner.view.collapsible_header(cx, ids!(group_choices_toggle)).set_expanded(cx, false, Animate::No);
        inner.view.collapsible_header(cx, ids!(flow_payload_toggle)).set_expanded(cx, false, Animate::No);
        inner.view.widget(cx, ids!(group_section)).set_visible(cx, true);
        inner.view.widget(cx, ids!(group_choices_toggle)).set_visible(cx, info.requests.len() > 1);
        inner.view.widget(cx, ids!(group_choices_body)).set_visible(cx, false);
        inner.view.widget(cx, ids!(flow_section)).set_visible(cx, has_details);
        for id in [ids!(prompt_message), ids!(prompt_write_warning), ids!(prompt_context), ids!(prompt_reason), ids!(prompt_tool), ids!(flow_destination_row), ids!(flow_payload_body), ids!(flow_payload)] {
            inner.view.widget(cx, id).set_visible(cx, false);
        }
        let (name, icon) = info.requests.first().map(PermissionPromptInfo::identity).unwrap_or(("Mini-app", ""));
        let agent = info.requests.iter().any(|request| match request {
            PermissionPromptInfo::Ordinary(info) => info.agent,
            PermissionPromptInfo::Flow(info) => info.agent,
        });
        inner.view.label(cx, ids!(prompt_title)).set_text(cx, &review_text(&format!("{icon} {name} needs permission")));
        inner.view.label(cx, ids!(prompt_blurb)).set_text(cx, if agent { "To answer you, the AI needs permission to:" }
            else { "To continue, this app needs permission to:" });
        inner.view.label(cx, ids!(permission_help)).set_text(cx, &review_text(&permission_help(name, agent, true)));
        inner.configure_group_selection(cx, true);
        inner.view.redraw(cx);
    }

    /// Refreshes each request's trusted space ancestry while keeping all choices.
    pub fn update_group_scope_targets(&self, cx: &mut Cx, targets: &[(u64, Vec<(String, Vec<String>)>)]) {
        let Some(mut inner) = self.borrow_mut() else { return };
        if inner.group_id.is_none() { return }
        for request in &mut inner.group_requests {
            if let Some((_, updated)) = targets.iter().find(|(id, _)| *id == request.prompt_id()) {
                match request {
                    PermissionPromptInfo::Ordinary(info) => info.scope_targets = updated.clone(),
                    PermissionPromptInfo::Flow(info) => info.scope_targets = updated.clone(),
                }
            }
        }
        inner.configure_group_selection(cx, false);
    }

    /// Refreshes trusted space ancestry without changing the user's choices.
    pub fn update_scope_targets(&self, cx: &mut Cx, targets: &[(String, Vec<String>)]) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.scope_targets = targets.to_vec();
        inner.refresh_duration(cx);
        inner.view.redraw(cx);
    }

    pub fn set_space_membership_ready(&self, cx: &mut Cx, ready: bool) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.space_membership_ready = ready;
        inner.refresh_duration(cx);
        inner.view.redraw(cx);
    }
}

fn review_text(payload: &str) -> String {
    let mut displayed = String::with_capacity(payload.len());
    for character in payload.chars() {
        if (character.is_control() && !matches!(character, '\n' | '\t'))
            || matches!(character, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
        {
            use std::fmt::Write;
            let _ = write!(displayed, "\\u{:04x}", character as u32);
        } else { displayed.push(character); }
    }
    displayed
}

const ROOM_WRITE_WARNING: &str = "Approving also turns on room changes. Other apps still need their own permission.";

fn ordinary_action(info: &PromptInfo) -> String {
    let action = info.capability.as_deref().filter(|action| !action.trim().is_empty()).unwrap_or_else(|| permission_action_text(info.perm));
    if info.enable_writes && info.message_preview.is_none() {
        format!("{action}. {ROOM_WRITE_WARNING}")
    } else { action.to_string() }
}

/// Permission categories are useful in settings; a popup needs a concrete verb.
fn permission_action_text(permission: Permission) -> &'static str {
    match permission {
        Permission::Network => "Connect to a website",
        Permission::Location => "Use your current location",
        Permission::Notifications => "Show you notifications",
        Permission::ClipboardRead => "Read text you copied to the clipboard",
        Permission::Ipc => "Exchange messages with another app",
        Permission::MatrixRoomRead => "Read messages in this room",
        Permission::MatrixRoomSend => "Post messages in this room as you",
        Permission::ClipboardWrite => "Copy text to the clipboard",
        Permission::OpenUrl => "Open a link in your browser",
        Permission::Files => "Open or save files you choose",
        Permission::Share => "Share text or files using another app",
        Permission::Auth => "Start a sign-in request",
        Permission::MatrixRoomInfo => "Read this room's name and details",
        Permission::MatrixProfile => "Read your name and account ID",
        Permission::DeviceInfo => "Read basic details about your device",
        Permission::Camera => "Take a photo with your camera",
        Permission::Microphone => "Record audio with your microphone",
        Permission::MatrixAccountRead => "Read your account settings and device details",
        Permission::MatrixAccountWrite => "Change your account profile or settings",
        Permission::MatrixUsers => "Look up people's profiles",
        Permission::MatrixRoomWatch => "Receive updates when this room changes",
        Permission::MatrixRoomInteract => "React to messages or mark this room as read as you",
        Permission::MatrixRoomAppData => "Save and read this app's shared data in this room",
        Permission::MatrixRoomManage => "Change this room's settings or pinned messages",
        Permission::MatrixRoomInvite => "Invite people to this room as you",
        Permission::MatrixMedia => "Fetch, upload, or post images and files in the rooms you approve",
        Permission::MatrixRoomsList => "See your rooms, chats, and invitations",
        Permission::MatrixRoomsRead => "Read messages in the rooms you approve",
        Permission::MatrixRoomsSend => "Post messages as you in the rooms you approve",
        Permission::MatrixMembership => "Join or leave rooms, answer invitations, or start chats as you",
        Permission::MatrixSpaces => "See your spaces and the rooms inside them",
        Permission::RobrixNavigation => "Open a room, message, or screen in Robrix",
        Permission::RobrixComposer => "Put draft text or attachments in the message box for you to review",
        Permission::RobrixUi => "Resize or move this app's pane",
        Permission::RobrixPreferences => "Read Robrix's display settings",
        Permission::RobrixObserve => "See which room or screen you open",
        Permission::AppGeneration => "Build and run a new mini-app",
        Permission::McpTools => "Add a tool that your AI can use",
        Permission::AppLaunch => "Open an installed mini-app",
    }
}

fn ordinary_context(cx: &mut Cx, info: &PromptInfo) -> String {
    let names = if cx.has_global::<RoomsListRef>() { cx.get_global::<RoomsListRef>().permission_targets() } else { Vec::new() };
    let room_name = |id: &str| names.iter().find(|(room, _, _)| room == id).map(|(_, name, _)| name.clone())
        .unwrap_or_else(|| if id.starts_with('!') { "Selected room".into() } else { id.into() });
    let mut context = Vec::new();
    match info.scope.as_ref() {
        Some(RoomScope::AllRooms) if info.collection => context.push("Your rooms and spaces. Blocked rooms stay protected.".to_string()),
        Some(RoomScope::Selection { rooms, spaces }) => {
            let targets = rooms.iter().map(|id| room_name(id))
                .chain(spaces.iter().map(|id| format!("{} and its rooms", room_name(id)))).collect::<Vec<_>>();
            let label = if !spaces.is_empty() { "Rooms and spaces" } else if targets.len() == 1 { "Room" } else { "Rooms" };
            context.push(format!("{label}: {}", targets.join(", ")));
        },
        _ => {},
    }
    if let Some(origin) = info.network_url.as_deref().and_then(|url| NetworkScope::from_url(url, NetworkScopeKind::Origin).ok()) {
        if let NetworkScope::Origin(origin) = origin { context.push(format!("Website: {origin}")); }
    } else if info.perm == Permission::Network {
        context.push("This app did not provide a valid website address. Choose Deny, then check the address in the app.".into());
    }
    context.join("\n")
}

fn flow_target(info: &FlowPromptInfo) -> String {
    info.destination.strip_prefix("Room: ").map(|room| format!("In {room}"))
        .unwrap_or_else(|| info.destination.clone())
}

fn tool_text(tool: &ToolPreview) -> String {
    let mut text = format!("{}\n\n{}", tool.name, tool.description);
    for (name, ty, description) in &tool.args { text.push_str(&format!("\n• {name} ({ty}): {description}")); }
    text
}

fn permission_help(app_name: &str, agent: bool, grouped: bool) -> String {
    let denied = if grouped { "Deny stops these requests." } else { "Deny stops this request." };
    if agent {
        format!("{denied} To ask again or remove approvals, open Mini Apps > AI room permissions.")
    } else {
        format!("{denied} To ask again, try the action again or close and reopen the app. Change approvals in Mini Apps > {app_name} > Permissions.")
    }
}

fn scope_covers_targets(scope: &RoomScope, targets: &[(String, Vec<String>)]) -> bool {
    let nonempty = match scope {
        RoomScope::AllRooms => true,
        RoomScope::Selection { rooms, spaces } => !rooms.is_empty() || !spaces.is_empty(),
    };
    nonempty && targets.iter().all(|(target, ancestors)| match scope {
        RoomScope::AllRooms => true,
        RoomScope::Selection { rooms, spaces } => rooms.contains(target) || spaces.contains(target)
            || spaces.iter().any(|space| ancestors.contains(space)),
    })
}

fn popup_scope_summary(cx: &mut Cx, scope: &RoomScope) -> String {
    match scope {
        RoomScope::AllRooms => "All rooms and spaces, including ones you join later. Blocked rooms stay protected.".into(),
        RoomScope::Selection { rooms, spaces } => {
            let targets = if cx.has_global::<RoomsListRef>() { cx.get_global::<RoomsListRef>().permission_targets() } else { Vec::new() };
            let name = |id: &str| targets.iter().find(|(target, _, _)| target == id).map(|(_, name, _)| name.clone())
                .unwrap_or_else(|| if id.starts_with('!') { "Selected room".into() } else { id.into() });
            let names = rooms.iter().map(|room| name(room))
                .chain(spaces.iter().map(|space| format!("{} and its rooms", name(space)))).collect::<Vec<_>>();
            format!("Applies in {}. Blocked rooms stay protected.", names.join(", "))
        },
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
    #[rust] popup_scope: bool,
    #[rust] popup_spaces_only: bool,
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
        if !self.popup_scope && let Some(index) = self.view.permission_choices(cx, ids!(scope_choice)).changed(actions) {
            changed = true;
            self.scope_choice_valid = index < 2;
            self.all_rooms = index == 1;
            self.update_target_visibility(cx);
        }
        if !self.popup_scope && self.view.button(cx, ids!(choose_targets)).clicked(actions) {
            changed = true;
            self.targets_expanded = !self.targets_expanded;
            self.update_target_visibility(cx);
        }
        if !self.popup_scope && let Some(index) = self.view.permission_choices(cx, ids!(duration_choice)).changed(actions) {
            changed = true;
            self.duration = index;
            self.view.widget(cx, ids!(session_origin_section)).set_visible(cx, self.choose_origin && index == 0);
        }
        if !self.popup_scope && let Some(index) = self.view.permission_choices(cx, ids!(session_origin)).changed(actions) {
            changed = true;
            self.session_origin_index = index;
        }
        if !self.popup_scope && let Some(index) = self.view.permission_choices(cx, ids!(network_choice)).changed(actions) {
            changed = true;
            self.network_kind = match index { 0 => 0, 1 => 1, 2 => 3, _ => usize::MAX };
            self.view.permission_choices(cx, ids!(network_advanced_choice)).set_selected_item(cx, usize::MAX);
            self.update_network_visibility(cx);
            self.update_network_summary(cx);
        }
        if !self.popup_scope && let Some(index) = self.view.permission_choices(cx, ids!(network_advanced_choice)).changed(actions) {
            changed = true;
            self.network_kind = match index { 0 => 2, 1 => 4, _ => usize::MAX };
            self.view.permission_choices(cx, ids!(network_choice)).set_selected_item(cx, usize::MAX);
            self.update_network_visibility(cx);
            self.update_network_summary(cx);
        }
        if !self.popup_scope && self.view.button(cx, ids!(change_network)).clicked(actions) {
            self.network_expanded = !self.network_expanded;
            self.update_network_visibility(cx);
        }
        if !self.popup_scope && self.view.button(cx, ids!(more_network)).clicked(actions) {
            self.advanced_network_expanded = !self.advanced_network_expanded;
            self.update_network_visibility(cx);
        }
        if !self.popup_scope && let Some(value) = self.view.text_input(cx, ids!(network_value)).changed(actions) {
            changed = true;
            self.network_value = value;
            self.update_network_summary(cx);
        }
        if let Some(filter) = self.view.text_input(cx, ids!(target_filter)).changed(actions) {
            changed = true;
            let filter = filter.to_lowercase();
            self.filtered = self.targets.iter().enumerate()
                .filter(|(_, (id, name, space))| (!self.popup_spaces_only || *space) && (name.to_lowercase().contains(&filter) || id.contains(&filter)))
                .map(|(index, _)| index).collect();
            if !self.popup_scope { self.view.portal_list(cx, ids!(targets)).set_first_id_and_scroll(0, 0.0); }
        }
        let target_changes = if self.popup_scope {
            self.view.flat_list(cx, ids!(popup_targets)).items_with_actions(actions).into_iter()
                .filter_map(|(id, row)| {
                    let target_index = usize::try_from(id.0.checked_sub(1)?).ok()?;
                    self.filtered.contains(&target_index).then_some(target_index)
                        .zip(row.as_check_box().changed(actions))
                }).collect::<Vec<_>>()
        } else {
            self.view.portal_list(cx, ids!(targets)).items_with_actions(actions).into_iter()
                .filter_map(|(row_index, row)| self.filtered.get(row_index).copied()
                    .zip(row.as_check_box().changed(actions))).collect::<Vec<_>>()
        };
        for (target_index, checked) in target_changes {
            if let Some((id, _, is_space)) = self.targets.get(target_index) {
                changed = true;
                if self.popup_spaces_only && checked { self.spaces.clear(); }
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
            } else if let Some(mut list) = widget.as_flat_list().borrow_mut() {
                for &target_index in &self.filtered {
                    let Some((id, name, is_space)) = self.targets.get(target_index) else { continue };
                    let Some(row) = list.item(cx, LiveId(target_index as u64 + 1), id!(target)) else { continue };
                    row.set_text(cx, &format!("{} · {}", name, if *is_space { "Space and its rooms" } else { "Room" }));
                    let selected = if *is_space { &self.spaces } else { &self.rooms };
                    row.as_check_box().set_active(cx, selected.contains(id), Animate::No);
                    row.draw_all(cx, scope);
                }
                list.items.retain_visible();
            }
        }
        DrawStep::done()
    }
}

impl PermissionScopeEditor {
    fn update_target_visibility(&self, cx: &mut Cx) {
        self.view.widget(cx, ids!(choose_targets)).set_visible(cx, !self.room_policy && !self.popup_scope);
        self.view.widget(cx, ids!(selected_targets)).set_visible(cx, self.targets_expanded || self.popup_scope);
        self.view.widget(cx, ids!(target_checklist)).set_visible(cx, !self.all_rooms);
        self.view.widget(cx, ids!(editor_targets)).set_visible(cx, !self.popup_scope);
        self.view.widget(cx, ids!(popup_targets_section)).set_visible(cx, self.popup_scope);
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
        self.view.widget(cx, ids!(target_empty_row)).set_visible(cx, self.filtered.is_empty());
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
        inner.popup_scope = false;
        inner.popup_spaces_only = false;
        inner.view.widget(cx, ids!(scope_heading)).set_visible(cx, true);
        inner.view.widget(cx, ids!(target_summary)).set_visible(cx, true);
        inner.all_rooms = room_id.is_none() && !policy;
        inner.scope_choice_valid = true;
        inner.view.permission_choices(cx, ids!(scope_choice)).set_selected_item(cx, usize::from(inner.all_rooms));
        inner.view.widget(cx, ids!(scope_choice_section)).set_visible(cx, !policy);
        inner.targets_expanded = !inner.all_rooms && (policy || room_id.is_none());
        inner.update_target_visibility(cx);
        inner.view.text_input(cx, ids!(target_filter)).set_text(cx, "");
        inner.view.text_input(cx, ids!(target_filter)).set_empty_text(cx, "Find rooms or spaces…".into());
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

    /// Reuses the checklist inside the popup without another scope or duration editor.
    fn configure_popup(&self, cx: &mut Cx, scope: &RoomScope, spaces_only: bool) {
        self.configure(cx, None, None, None, false, false);
        let selected = match scope {
            RoomScope::Selection { rooms, spaces } => RoomScope::Selection {
                rooms: if spaces_only { Vec::new() } else { rooms.clone() },
                spaces: if spaces_only { spaces.iter().take(1).cloned().collect() } else { spaces.clone() },
            },
            RoomScope::AllRooms => RoomScope::Selection { rooms: Vec::new(), spaces: Vec::new() },
        };
        self.set_scope(cx, &selected);
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.popup_scope = true;
        inner.popup_spaces_only = spaces_only;
        inner.filtered = inner.targets.iter().enumerate().filter(|(_, (_, _, space))| !spaces_only || *space)
            .map(|(index, _)| index).collect();
        for id in [ids!(scope_heading), ids!(target_summary), ids!(scope_choice_section), ids!(duration_section), ids!(network_section)] {
            inner.view.widget(cx, id).set_visible(cx, false);
        }
        let hint = if spaces_only { "Find a space…" } else { "Find rooms or spaces…" };
        inner.view.text_input(cx, ids!(target_filter)).set_empty_text(cx, hint.into());
        inner.update_target_visibility(cx);
        inner.update_summary(cx);
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
            inner.filtered = inner.targets.iter().enumerate().filter(|(_, (_, _, space))| !inner.popup_spaces_only || *space)
                .map(|(index, _)| index).collect();
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

    fn prompt() -> (Cx, MiniAppPermissionPromptRef) {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let prompt = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            crate::shared::script_mod(vm);
            super::super::permission_choices::script_mod(vm);
            super::super::permission_message_preview::script_mod(vm);
            super::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.MiniAppPermissionPrompt {} });
            WidgetRef::script_from_value(vm, value).as_mini_app_permission_prompt()
        });
        (cx, prompt)
    }

    fn editor() -> (Cx, PermissionScopeEditorRef) {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let widget = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            crate::shared::script_mod(vm);
            super::super::permission_choices::script_mod(vm);
            super::super::permission_message_preview::script_mod(vm);
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
    fn popup_target_selection_keeps_room_identity_when_the_checklist_is_filtered() {
        let (mut cx, editor) = editor();
        let rooms = vec!["!alpha:example.org".to_string(), "!beta:example.org".to_string()];
        editor.configure_popup(&mut cx, &RoomScope::Selection { rooms: rooms.clone(), spaces: Vec::new() }, false);
        let inner = editor.borrow().unwrap();
        let list = inner.view.flat_list(&cx, ids!(popup_targets));
        let filter = inner.view.text_input(&cx, ids!(target_filter)).widget_uid();
        let alpha = inner.targets.iter().position(|(id, _, _)| id == &rooms[0]).unwrap();
        let beta = inner.targets.iter().position(|(id, _, _)| id == &rooms[1]).unwrap();
        drop(inner);
        let alpha_row = list.item(&mut cx, LiveId(alpha as u64 + 1), id!(target)).unwrap();
        let beta_row = list.item(&mut cx, LiveId(beta as u64 + 1), id!(target)).unwrap();
        let change = cx.capture_actions(|cx| cx.widget_action(filter, TextInputAction::Changed("!beta".into())));
        editor.borrow_mut().unwrap().handle_event(&mut cx, &Event::Actions(change), &mut Scope::empty());
        for row in [&beta_row, &alpha_row] {
            let change = cx.capture_actions(|cx| cx.group_widget_actions(list.widget_uid(), row.widget_uid(), |cx| {
                cx.widget_action(row.widget_uid(), CheckBoxAction::Change(false));
            }));
            editor.borrow_mut().unwrap().handle_event(&mut cx, &Event::Actions(change), &mut Scope::empty());
        }
        assert_eq!(editor.selection().unwrap().scope, RoomScope::room(&rooms[0]),
            "a filtered row selects its own room, and a stale hidden row cannot alter another room's permission");
        editor.configure(&mut cx, Some(&rooms[0]), None, None, false, false);
        let inner = editor.borrow().unwrap();
        assert!(inner.view.widget(&cx, ids!(editor_targets)).visible());
        assert!(!inner.view.widget(&cx, ids!(popup_targets_section)).visible());
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

    fn ordinary_info() -> PromptInfo {
        PromptInfo {
            prompt_id: 17, app_name: "Search".into(), app_icon: "🔎".into(), perm: Permission::MatrixRoomRead,
            reason: Some("Search messages in this room.".into()), capability: Some("Search messages".into()),
            agent: false, tool: None, message_preview: None, room_id: Some("!target:example.org".into()), origin_room_id: Some("!origin:example.org".into()),
            network_url: None, can_allow_once: true, collection: false, enable_writes: false, scope: Some(RoomScope::room("!target:example.org")),
            scope_targets: vec![("!target:example.org".into(), vec!["!parent:example.org".into()])],
        }
    }

    fn answer(prompt: &MiniAppPermissionPromptRef, cx: &mut Cx, duration: usize, deny: bool) -> PermissionPromptResponse {
        let inner = prompt.borrow().unwrap();
        let dropdown = inner.view.drop_down2(cx, ids!(approval_duration)).widget_uid();
        let button = inner.view.button(cx, if deny { ids!(not_now_button) } else { ids!(allow_button) }).widget_uid();
        drop(inner);
        let selected = cx.capture_actions(|cx| cx.widget_action(dropdown, DropDown2Action::Select(duration)));
        prompt.borrow_mut().unwrap().handle_event(cx, &Event::Actions(selected), &mut Scope::empty());
        let click = cx.capture_actions(|cx| cx.widget_action(button, ButtonAction::Clicked(Default::default())));
        let actions = cx.capture_actions(|cx| prompt.borrow_mut().unwrap().handle_event(cx, &Event::Actions(click), &mut Scope::empty()));
        actions.iter().find_map(|action| action.downcast_ref::<PermissionPromptResponse>().cloned()).expect("one decision response")
    }

    fn answer_for_duration(prompt: &MiniAppPermissionPromptRef, cx: &mut Cx, duration: ApprovalDuration) -> PermissionPromptResponse {
        let index = prompt.borrow().unwrap().durations.iter().position(|candidate| *candidate == duration).expect("duration is available");
        answer(prompt, cx, index, false)
    }

    #[test]
    fn truncated_message_details_expand_for_ordinary_requests_and_reset_between_prompts() {
        let (mut cx, prompt) = prompt();
        let body = "First line\nSecond line\nThird line\nFourth line remains available for review";
        let mut message = ordinary_info();
        message.perm = Permission::MatrixRoomSend;
        message.capability = Some("Post this message:".into());
        message.message_preview = Some(PermissionMessagePreview { body: body.into(), formatted_html: Some("<p>First line</p>".into()) });
        let ordinary_group = PermissionPromptGroupInfo {
            group_id: 71,
            requests: vec![PermissionPromptInfo::Ordinary(message.clone())],
        };
        for grouped in [false, true] {
            if grouped { prompt.show_group(&mut cx, &ordinary_group); }
            else { prompt.show(&mut cx, &message); }
            let inner = prompt.borrow().unwrap();
            assert!(!inner.flow_mode, "reviewing text must not change the permission decision kind");
            assert!(inner.view.widget(&cx, ids!(flow_section)).visible());
            assert!(!inner.view.widget(&cx, ids!(flow_payload)).visible());
            assert!(inner.view.label(&cx, ids!(flow_payload)).text().contains(body));
            let details = inner.view.collapsible_header(&cx, ids!(flow_payload_toggle)).widget_uid();
            drop(inner);
            let expanded = cx.capture_actions(|cx| cx.widget_action(details, crate::shared::collapsible_header::CollapsibleHeaderAction::ExpansionChanged(true)));
            prompt.borrow_mut().unwrap().handle_event(&mut cx, &Event::Actions(expanded), &mut Scope::empty());
            assert!(prompt.borrow().unwrap().view.widget(&cx, ids!(flow_payload)).visible());
            prompt.show(&mut cx, &ordinary_info());
            let inner = prompt.borrow().unwrap();
            assert!(!inner.view.widget(&cx, ids!(prompt_message)).visible());
            assert!(!inner.view.widget(&cx, ids!(flow_section)).visible());
            assert!(!inner.view.widget(&cx, ids!(flow_payload)).visible());
            assert!(inner.view.label(&cx, ids!(flow_payload)).text().is_empty());
            drop(inner);
            assert_eq!(answer_for_duration(&prompt, &mut cx, ApprovalDuration::Session).prompt_id, 17);
        }
    }

    fn group_answer(prompt: &MiniAppPermissionPromptRef, cx: &mut Cx, duration: usize, deny: bool) -> PermissionPromptGroupResponse {
        let inner = prompt.borrow().unwrap();
        let dropdown = inner.view.drop_down2(cx, ids!(approval_duration)).widget_uid();
        let button = inner.view.button(cx, if deny { ids!(not_now_button) } else { ids!(allow_button) }).widget_uid();
        drop(inner);
        let selected = cx.capture_actions(|cx| cx.widget_action(dropdown, DropDown2Action::Select(duration)));
        prompt.borrow_mut().unwrap().handle_event(cx, &Event::Actions(selected), &mut Scope::empty());
        let click = cx.capture_actions(|cx| cx.widget_action(button, ButtonAction::Clicked(Default::default())));
        let actions = cx.capture_actions(|cx| prompt.borrow_mut().unwrap().handle_event(cx, &Event::Actions(click), &mut Scope::empty()));
        let responses = actions.iter().filter_map(|action| action.downcast_ref::<PermissionPromptGroupResponse>().cloned()).collect::<Vec<_>>();
        assert_eq!(responses.len(), 1, "one decision resolves the entire group");
        responses.into_iter().next().unwrap()
    }

    #[test]
    fn agent_flow_prompts_use_ai_permissions_help_without_ordinary_requests() {
        let (mut cx, prompt) = prompt();
        let mut info = FlowPromptInfo {
            prompt_id: 33, app_name: "Room AI".into(), app_icon: "🤖".into(), agent: true,
            action: "Send your answer to this room".into(), destination: "Room: Selected room".into(),
            sources: vec!["Room messages".into()], payload: "{\"message\":\"Your answer\"}".into(),
            message_preview: None, write_warning: None, allow_once: true, allow_lasting: true, scope: None, scope_targets: Vec::new(), room_id: None, origin_room_id: None,
        };
        prompt.show_flow(&mut cx, &info);
        let help = prompt.borrow().unwrap().view.label(&cx, ids!(permission_help)).text();
        assert!(help.contains("Deny stops this request."));
        assert!(help.contains("Mini Apps > AI room permissions"));
        assert!(!help.contains("Room AI > Permissions"));

        let mut second = info.clone();
        second.prompt_id = 34;
        prompt.show_group(&mut cx, &PermissionPromptGroupInfo {
            group_id: 7, requests: vec![PermissionPromptInfo::Flow(info.clone()), PermissionPromptInfo::Flow(second)],
        });
        let inner = prompt.borrow().unwrap();
        assert_eq!(inner.view.label(&cx, ids!(prompt_blurb)).text(), "To answer you, the AI needs permission to:");
        let help = inner.view.label(&cx, ids!(permission_help)).text();
        assert!(help.contains("Deny stops these requests."));
        assert!(help.contains("Mini Apps > AI room permissions"));
        assert!(!help.contains("Room AI > Permissions"));
        drop(inner);
        let response = group_answer(&prompt, &mut cx, 0, false);
        assert_eq!(response.group_id, 7);
        assert_eq!(response.responses.iter().map(|response| response.prompt_id).collect::<Vec<_>>(), [33, 34]);
        assert!(response.responses.iter().all(|response| matches!(response.answer, PermissionPromptAction::AllowFlowOnce)));

        info.agent = false;
        prompt.show_flow(&mut cx, &info);
        let help = prompt.borrow().unwrap().view.label(&cx, ids!(permission_help)).text();
        assert!(help.contains("Mini Apps > Room AI > Permissions"));
        assert!(!help.contains("AI room permissions"), "a new app request must discard the previous agent identity");
    }

    #[test]
    fn grouped_choices_require_compatible_duration_and_resolve_every_request_once() {
        let (mut cx, prompt) = prompt();
        let mut ongoing = ordinary_info();
        ongoing.can_allow_once = false;
        let flow = FlowPromptInfo {
            prompt_id: 35, app_name: "Search".into(), app_icon: "🔎".into(), agent: false,
            action: "Open this message".into(), destination: "Robrix".into(), sources: Vec::new(), payload: "{}".into(),
            message_preview: None, write_warning: None, allow_once: true, allow_lasting: false, scope: None, scope_targets: Vec::new(), room_id: None, origin_room_id: None,
        };
        let info = PermissionPromptGroupInfo {
            group_id: 8, requests: vec![PermissionPromptInfo::Ordinary(ongoing), PermissionPromptInfo::Flow(flow)],
        };
        prompt.show_group(&mut cx, &info);
        let inner = prompt.borrow().unwrap();
        assert!(inner.view.widget(&cx, ids!(allow_button)).disabled(&cx));
        assert!(!inner.view.widget(&cx, ids!(approval_duration_section)).visible());
        assert_eq!(inner.view.label(&cx, ids!(group_selection_summary)).text(),
            "These requests need different approval durations. Choose permissions individually to approve one at a time.");
        let header = inner.view.collapsible_header(&cx, ids!(group_choices_toggle)).widget_uid();
        let list = inner.view.flat_list(&cx, ids!(group_choices));
        drop(inner);
        let expanded = cx.capture_actions(|cx| cx.widget_action(header,
            crate::shared::collapsible_header::CollapsibleHeaderAction::ExpansionChanged(true)));
        prompt.borrow_mut().unwrap().handle_event(&mut cx, &Event::Actions(expanded), &mut Scope::empty());
        let ongoing_row = list.item(&mut cx, LiveId(1), id!(Request)).unwrap();
        let changed = cx.capture_actions(|cx| cx.group_widget_actions(list.widget_uid(), ongoing_row.widget_uid(), |cx| {
            cx.widget_action(ongoing_row.widget_uid(), CheckBoxAction::Change(false));
        }));
        prompt.borrow_mut().unwrap().handle_event(&mut cx, &Event::Actions(changed), &mut Scope::empty());
        assert_eq!(prompt.borrow().unwrap().durations, [ApprovalDuration::Choose, ApprovalDuration::Once]);
        assert!(prompt.borrow().unwrap().view.widget(&cx, ids!(allow_button)).disabled(&cx), "a compatible duration still needs an explicit choice");
        let response = group_answer(&prompt, &mut cx, 1, false);
        assert_eq!(response.group_id, 8);
        assert_eq!(response.responses.len(), 2);
        assert_eq!(response.responses[0].prompt_id, 17);
        assert!(matches!(response.responses[0].answer, PermissionPromptAction::NotNow));
        assert_eq!(response.responses[1].prompt_id, 35);
        assert!(matches!(response.responses[1].answer, PermissionPromptAction::AllowFlowOnce));

        prompt.show_group(&mut cx, &info);
        assert_eq!(prompt.borrow().unwrap().group_selected, [true, true]);
        let response = group_answer(&prompt, &mut cx, 0, true);
        assert_eq!(response.responses.iter().map(|response| response.prompt_id).collect::<Vec<_>>(), [17, 35]);
        assert!(response.responses.iter().all(|response| matches!(response.answer, PermissionPromptAction::NotNow)));
    }

    #[test]
    fn one_simple_prompt_emits_selected_duration_with_captured_room_scope() {
        let (mut cx, prompt) = prompt();
        let info = ordinary_info();
        for (index, duration) in [(0, None), (1, Some(GrantDuration::RoomSession)), (2, Some(GrantDuration::RobrixSession)), (3, Some(GrantDuration::Always))] {
            prompt.show(&mut cx, &info);
            let inner = prompt.borrow().unwrap();
            assert_eq!(inner.duration, 2, "a new request defaults to the Robrix session");
            assert_eq!(inner.view.button(&cx, ids!(allow_button)).text(), "Approve");
            assert_eq!(inner.view.button(&cx, ids!(not_now_button)).text(), "Deny");
            assert!(inner.view.widget(&cx, ids!(approval_duration)).visible());
            assert!(inner.view.widget(&cx, ids!(scope_editor)).is_empty(), "the decision modal has no scope wizard");
            assert!(inner.view.label(&cx, ids!(prompt_blurb)).text().contains("Search messages"));
            assert!(!inner.view.label(&cx, ids!(prompt_context)).text().contains("!target"), "raw room ids are not request explanations");
            drop(inner);
            let response = answer(&prompt, &mut cx, index, false);
            assert_eq!(response.prompt_id, info.prompt_id);
            match (duration, response.answer) {
                (None, PermissionPromptAction::AllowOnce) => {},
                (Some(expected), PermissionPromptAction::AllowScoped { scope, duration, network }) => {
                    assert_eq!(scope, RoomScope::room("!target:example.org"));
                    assert_eq!(duration, expected);
                    assert!(network.is_none());
                }
                (_, other) => panic!("wrong approval: {other:?}"),
            }
        }
        assert!(matches!(answer(&prompt, &mut cx, 2, true).answer, PermissionPromptAction::NotNow));
    }

    #[test]
    fn website_and_collection_scopes_are_captured_without_an_editor() {
        let (mut cx, prompt) = prompt();
        let mut info = ordinary_info();
        info.perm = Permission::Network;
        info.scope = None;
        info.network_url = Some("https://Example.org:443/changing/path?value=2".into());
        prompt.show(&mut cx, &info);
        assert!(!prompt.borrow().unwrap().view.widget(&cx, ids!(spatial_section)).visible());
        assert!(prompt.borrow().unwrap().view.label(&cx, ids!(prompt_context)).text().contains("Website: https://example.org"));
        assert!(matches!(answer_for_duration(&prompt, &mut cx, ApprovalDuration::Session).answer,
            PermissionPromptAction::AllowScoped { scope, duration: GrantDuration::RobrixSession, network: Some(NetworkScope::Origin(origin)) }
                if scope == RoomScope::room("!target:example.org") && origin == "https://example.org"));
        info.collection = true;
        info.scope = Some(RoomScope::AllRooms);
        info.perm = Permission::MatrixRoomsList;
        info.network_url = None;
        prompt.show(&mut cx, &info);
        assert!(prompt.borrow().unwrap().view.label(&cx, ids!(prompt_context)).text().contains("Your rooms and spaces"));
        assert!(matches!(answer_for_duration(&prompt, &mut cx, ApprovalDuration::Session).answer,
            PermissionPromptAction::AllowScoped { scope: RoomScope::AllRooms, .. }));
        info.collection = false;
        info.scope = Some(RoomScope::room("!target:example.org"));
        prompt.show(&mut cx, &info);
        assert!(matches!(answer_for_duration(&prompt, &mut cx, ApprovalDuration::Session).answer,
            PermissionPromptAction::AllowScoped { scope, .. } if scope == RoomScope::room("!target:example.org")));
    }

    #[test]
    fn selected_rooms_are_captured_without_broadening_to_a_collection() {
        let (mut cx, prompt) = prompt();
        let mut info = ordinary_info();
        info.collection = true;
        let scope = RoomScope::Selection { rooms: vec!["First room".into(), "Second room".into()], spaces: Vec::new() };
        info.scope = Some(scope.clone());
        info.scope_targets = vec![("First room".into(), Vec::new()), ("Second room".into(), Vec::new())];
        prompt.show(&mut cx, &info);
        let context = prompt.borrow().unwrap().view.label(&cx, ids!(prompt_context)).text();
        assert!(context.contains("First room") && context.contains("Second room"));
        assert!(!context.contains("Your rooms and spaces"));
        assert!(matches!(answer_for_duration(&prompt, &mut cx, ApprovalDuration::Session).answer,
            PermissionPromptAction::AllowScoped { scope: captured, .. } if captured == scope));
    }

    #[test]
    fn subscriptions_and_write_enablement_offer_only_working_durations() {
        let (mut cx, prompt) = prompt();
        let mut info = ordinary_info();
        for (ongoing, enable_writes) in [(true, false), (false, true)] {
            info.can_allow_once = !ongoing;
            info.enable_writes = enable_writes;
            prompt.show(&mut cx, &info);
            assert_eq!(prompt.borrow().unwrap().durations, [ApprovalDuration::RoomSession, ApprovalDuration::Session, ApprovalDuration::Forever]);
            assert!(matches!(answer_for_duration(&prompt, &mut cx, ApprovalDuration::RoomSession).answer,
                PermissionPromptAction::AllowScoped { duration: GrantDuration::RoomSession, .. }));
            if enable_writes { assert!(prompt.borrow().unwrap().view.label(&cx, ids!(prompt_blurb)).text().contains("turns on room changes")); }
        }
        info.can_allow_once = true;
        info.enable_writes = false;
        prompt.show(&mut cx, &info);
        assert_eq!(prompt.borrow().unwrap().durations[0], ApprovalDuration::Once, "the next request restores its own choices");
    }

    #[test]
    fn room_duration_requires_an_origin_and_never_uses_the_target_room_as_its_lifetime() {
        let (mut cx, prompt) = prompt();
        let mut info = ordinary_info();
        info.can_allow_once = false;
        prompt.show(&mut cx, &info);
        assert_eq!(prompt.borrow().unwrap().duration_origin_room_id.as_deref(), Some("!origin:example.org"));
        prompt.borrow_mut().unwrap().select_spatial_scope(&mut cx, 1);
        assert!(matches!(answer_for_duration(&prompt, &mut cx, ApprovalDuration::RoomSession).answer,
            PermissionPromptAction::AllowScoped { scope: RoomScope::AllRooms, duration: GrantDuration::RoomSession, .. }));
        assert!(prompt.borrow().unwrap().view.label(&cx, ids!(duration_summary)).text().contains("last room tab or screen"));
        info.origin_room_id = None;
        prompt.show(&mut cx, &info);
        assert_eq!(prompt.borrow().unwrap().durations, [ApprovalDuration::Session, ApprovalDuration::Forever],
            "a target room does not attach an account-wide app to that room's lifetime");
        info.origin_room_id = Some(String::new());
        prompt.show(&mut cx, &info);
        assert!(!prompt.borrow().unwrap().durations.contains(&ApprovalDuration::RoomSession));
    }

    #[test]
    fn grouped_room_duration_requires_the_same_origin_and_rechecks_changed_selection() {
        let (mut cx, prompt) = prompt();
        let mut first = ordinary_info();
        first.can_allow_once = false;
        let mut second = first.clone();
        second.prompt_id = 18;
        let group = PermissionPromptGroupInfo { group_id: 9, requests: vec![
            PermissionPromptInfo::Ordinary(first.clone()), PermissionPromptInfo::Ordinary(second.clone()),
        ] };
        prompt.show_group(&mut cx, &group);
        let index = prompt.borrow().unwrap().durations.iter().position(|duration| *duration == ApprovalDuration::RoomSession).unwrap();
        let response = group_answer(&prompt, &mut cx, index, false);
        assert!(response.responses.iter().all(|response| matches!(response.answer,
            PermissionPromptAction::AllowScoped { duration: GrantDuration::RoomSession, .. })));

        second.origin_room_id = Some("!other:example.org".into());
        prompt.show_group(&mut cx, &PermissionPromptGroupInfo { group_id: 10, requests: vec![
            PermissionPromptInfo::Ordinary(first), PermissionPromptInfo::Ordinary(second),
        ] });
        assert!(!prompt.borrow().unwrap().durations.contains(&ApprovalDuration::RoomSession));
        {
            let mut inner = prompt.borrow_mut().unwrap();
            inner.group_selected = vec![true, false];
            inner.configure_group_selection(&mut cx, false);
        }
        let index = prompt.borrow().unwrap().durations.iter().position(|duration| *duration == ApprovalDuration::RoomSession).unwrap();
        group_answer(&prompt, &mut cx, index, false);
        {
            let mut inner = prompt.borrow_mut().unwrap();
            inner.group_selected = vec![false, true];
            inner.configure_group_selection(&mut cx, false);
        }
        let inner = prompt.borrow().unwrap();
        assert_eq!(inner.durations[inner.duration], ApprovalDuration::Choose,
            "switching the closing room requires a new duration choice");
        assert!(inner.view.widget(&cx, ids!(allow_button)).disabled(&cx));
    }

    #[test]
    fn room_bound_flow_and_mixed_groups_emit_room_lifetimes_only_when_lasting_is_supported() {
        let (mut cx, prompt) = prompt();
        let mut info = FlowPromptInfo {
            prompt_id: 39, app_name: "Search".into(), app_icon: "🔎".into(), agent: false,
            action: "Search another room".into(), destination: "Your Matrix server".into(), sources: vec!["Account data".into()],
            payload: "{}".into(), message_preview: None, write_warning: None, allow_once: true, allow_lasting: true,
            scope: Some(RoomScope::room("!target:example.org")), scope_targets: vec![("!target:example.org".into(), Vec::new())],
            room_id: Some("!target:example.org".into()), origin_room_id: Some("!origin:example.org".into()),
        };
        prompt.show_flow(&mut cx, &info);
        assert!(matches!(answer_for_duration(&prompt, &mut cx, ApprovalDuration::RoomSession).answer,
            PermissionPromptAction::AllowFlowScoped { duration: GrantDuration::RoomSession, .. }));
        info.scope = None;
        prompt.show_flow(&mut cx, &info);
        assert!(matches!(answer_for_duration(&prompt, &mut cx, ApprovalDuration::RoomSession).answer,
            PermissionPromptAction::AllowFlow { duration: GrantDuration::RoomSession }));
        prompt.show_group(&mut cx, &PermissionPromptGroupInfo { group_id: 11, requests: vec![
            PermissionPromptInfo::Ordinary(ordinary_info()), PermissionPromptInfo::Flow(info.clone()),
        ] });
        let index = prompt.borrow().unwrap().durations.iter().position(|duration| *duration == ApprovalDuration::RoomSession).unwrap();
        let response = group_answer(&prompt, &mut cx, index, false);
        assert!(matches!(response.responses[0].answer, PermissionPromptAction::AllowScoped { duration: GrantDuration::RoomSession, .. }));
        assert!(matches!(response.responses[1].answer, PermissionPromptAction::AllowFlow { duration: GrantDuration::RoomSession }));
        info.allow_lasting = false;
        prompt.show_flow(&mut cx, &info);
        assert_eq!(prompt.borrow().unwrap().durations, [ApprovalDuration::Once], "a room anchor never broadens restricted source consent");
    }

    #[test]
    fn missing_network_target_is_not_another_configuration_wizard() {
        let (mut cx, prompt) = prompt();
        let mut info = ordinary_info();
        info.perm = Permission::Network;
        prompt.show(&mut cx, &info);
        let inner = prompt.borrow().unwrap();
        assert!(!inner.request_valid);
        assert!(inner.view.widget(&cx, ids!(allow_button)).disabled(&cx));
        assert!(inner.view.label(&cx, ids!(prompt_context)).text().contains("check the address in the app"));
        assert!(inner.view.widget(&cx, ids!(scope_editor)).is_empty());
    }

    #[test]
    fn flow_prompt_has_one_decision_for_all_sources_and_selected_duration() {
        let (mut cx, prompt) = prompt();
        let info = FlowPromptInfo {
            prompt_id: 19, app_name: "Search".into(), app_icon: "🔎".into(), agent: false, action: "Search room messages and send your search text to your Matrix server.".into(),
            destination: "Your Matrix server".into(), sources: vec!["Account data".into(), "Unknown private data".into()],
            payload: "{\n  \"query\": \"nexus \u{202e}4\"\n}".into(), message_preview: None, write_warning: None, allow_once: true, allow_lasting: true,
            scope: None, scope_targets: Vec::new(), room_id: None, origin_room_id: None,
        };
        for (index, duration) in [(0, None), (1, Some(GrantDuration::RobrixSession)), (2, Some(GrantDuration::Always))] {
            prompt.show_flow(&mut cx, &info);
            let inner = prompt.borrow().unwrap();
            assert!(inner.view.widget(&cx, ids!(approval_duration)).visible());
            assert!(inner.view.widget(&cx, ids!(flow_sources)).is_empty(), "sources are not user choices");
            assert!(!inner.view.widget(&cx, ids!(flow_payload)).visible());
            assert!(inner.view.label(&cx, ids!(prompt_blurb)).text().contains("Search room messages"));
            assert!(inner.view.label(&cx, ids!(flow_payload)).text().contains("\\u202e"));
            drop(inner);
            let response = answer(&prompt, &mut cx, index, false);
            assert_eq!(response.prompt_id, info.prompt_id);
            match (duration, response.answer) {
                (None, PermissionPromptAction::AllowFlowOnce) => {},
                (Some(expected), PermissionPromptAction::AllowFlow { duration }) => assert_eq!(duration, expected),
                (_, other) => panic!("wrong flow approval: {other:?}"),
            }
        }
        let details = prompt.borrow().unwrap().view.collapsible_header(&cx, ids!(flow_payload_toggle)).widget_uid();
        let clicked = cx.capture_actions(|cx| cx.widget_action(details, crate::shared::collapsible_header::CollapsibleHeaderAction::ExpansionChanged(true)));
        prompt.borrow_mut().unwrap().handle_event(&mut cx, &Event::Actions(clicked), &mut Scope::empty());
        assert!(prompt.borrow().unwrap().view.widget(&cx, ids!(flow_payload)).visible());
        prompt.show(&mut cx, &ordinary_info());
        assert!(!prompt.borrow().unwrap().view.widget(&cx, ids!(flow_section)).visible());
        assert_eq!(answer_for_duration(&prompt, &mut cx, ApprovalDuration::Session).prompt_id, 17);
    }

    #[test]
    fn captured_legacy_request_is_one_plain_decision_without_a_source_editor() {
        let (mut cx, prompt) = prompt();
        let info = FlowPromptInfo {
            prompt_id: 27, app_name: "Search".into(), app_icon: "🔎".into(), agent: false, action: "Search for nexus in this room".into(),
            destination: "Your Matrix server: https://matrix.example.org".into(), sources: vec!["Account data".into(), "Unknown private data".into()],
            payload: "{\"query\":\"nexus\"}".into(), message_preview: None, write_warning: None, allow_once: true, allow_lasting: true,
            scope: None, scope_targets: Vec::new(), room_id: None, origin_room_id: None,
        };
        prompt.show_flow(&mut cx, &info);
        let inner = prompt.borrow().unwrap();
        assert_eq!(inner.durations, [ApprovalDuration::Once, ApprovalDuration::Session, ApprovalDuration::Forever]);
        assert_eq!(inner.view.drop_down2(&cx, ids!(approval_duration)).selected_item(), 1);
        assert!(inner.view.widget(&cx, ids!(approval_duration)).visible());
        assert!(!inner.view.widget(&cx, ids!(flow_destination)).visible());
        assert!(inner.view.label(&cx, ids!(flow_payload)).text().contains(&info.destination));
        assert!(inner.view.label(&cx, ids!(flow_payload)).text().contains(&info.payload));
        assert!(!inner.view.label(&cx, ids!(prompt_blurb)).text().contains("Unknown"));
        drop(inner);
        assert!(matches!(answer(&prompt, &mut cx, 0, false).answer, PermissionPromptAction::AllowFlowOnce));
        assert!(matches!(answer_for_duration(&prompt, &mut cx, ApprovalDuration::Session).answer, PermissionPromptAction::AllowFlow { duration: GrantDuration::RobrixSession }));
        assert!(matches!(answer(&prompt, &mut cx, 2, false).answer, PermissionPromptAction::AllowFlow { duration: GrantDuration::Always }));
    }

    #[test]
    fn restricted_source_request_cannot_select_lasting_approval() {
        let (mut cx, prompt) = prompt();
        let info = FlowPromptInfo {
            prompt_id: 28, app_name: "Search".into(), app_icon: "🔎".into(), agent: false, action: "Search for nexus in the selected room".into(),
            destination: "Room: Selected room".into(), sources: vec!["Data from a different account".into()],
            payload: "{\"query\":\"nexus\"}".into(), message_preview: None, write_warning: None, allow_once: true, allow_lasting: false,
            scope: None, scope_targets: Vec::new(), room_id: None, origin_room_id: None,
        };
        prompt.show_flow(&mut cx, &info);
        let inner = prompt.borrow().unwrap();
        assert_eq!(inner.durations, [ApprovalDuration::Once]);
        assert_eq!(inner.view.drop_down2(&cx, ids!(approval_duration)).selected_item(), 0);
        assert_eq!(inner.view.label(&cx, ids!(duration_summary)).text(), "Approval applies only to the request shown.");
        assert_eq!(inner.view.label(&cx, ids!(flow_destination)).text(), "In Selected room");
        assert!(inner.view.widget(&cx, ids!(flow_destination)).visible());
        drop(inner);
        assert!(matches!(answer(&prompt, &mut cx, 0, false).answer, PermissionPromptAction::AllowFlowOnce));
        assert!(matches!(answer(&prompt, &mut cx, 2, false).answer, PermissionPromptAction::AllowFlowOnce), "a stale duration cannot turn one-time review into lasting permission");
    }

    #[test]
    fn invalid_duration_events_preserve_choice_and_do_not_broaden_approval() {
        let (mut cx, prompt) = prompt();
        let mut info = ordinary_info();
        info.can_allow_once = false;
        prompt.show(&mut cx, &info);
        assert!(matches!(answer(&prompt, &mut cx, 99, false).answer,
            PermissionPromptAction::AllowScoped { duration: GrantDuration::RobrixSession, .. }));
        assert_eq!(prompt.borrow().unwrap().durations[prompt.borrow().unwrap().duration], ApprovalDuration::Session);
    }

    #[test]
    fn spatial_scope_requires_each_request_target_or_its_trusted_space() {
        let targets = vec![("First room".into(), vec!["Containing space".into()]),
            ("Second room".into(), vec!["Containing space".into()])];
        assert!(scope_covers_targets(&RoomScope::AllRooms, &targets));
        assert!(!scope_covers_targets(&RoomScope::room("First room"), &targets));
        assert!(scope_covers_targets(&RoomScope::Selection { rooms: Vec::new(), spaces: vec!["Containing space".into()] }, &targets));
        assert!(!scope_covers_targets(&RoomScope::Selection { rooms: Vec::new(), spaces: vec!["Other space".into()] }, &targets));
        assert!(!scope_covers_targets(&RoomScope::Selection { rooms: Vec::new(), spaces: Vec::new() }, &[]));
    }

    #[test]
    fn in_popup_space_picker_approves_only_a_covering_space_and_once_remains_exact() {
        let (mut cx, prompt) = prompt();
        prompt.show(&mut cx, &ordinary_info());
        prompt.borrow_mut().unwrap().select_spatial_scope(&mut cx, 2);
        let inner = prompt.borrow().unwrap();
        assert!(!inner.scope_valid);
        assert!(inner.view.widget(&cx, ids!(allow_button)).disabled(&cx));
        let picker = inner.view.permission_scope_editor(&cx, ids!(spatial_picker));
        drop(inner);
        for id in [ids!(scope_choice_section), ids!(duration_section), ids!(network_section), ids!(choose_targets)] {
            assert!(!picker.borrow().unwrap().view.widget(&cx, id).visible(), "the popup has no second scope, duration or network step");
        }
        let space = RoomScope::Selection { rooms: Vec::new(), spaces: vec!["!parent:example.org".into()] };
        picker.set_scope(&mut cx, &space);
        let changed = cx.capture_actions(|cx| cx.widget_action(picker.widget_uid(), PermissionScopeEditorAction::Changed));
        prompt.borrow_mut().unwrap().handle_event(&mut cx, &Event::Actions(changed), &mut Scope::empty());
        assert!(matches!(answer_for_duration(&prompt, &mut cx, ApprovalDuration::Session).answer,
            PermissionPromptAction::AllowScoped { scope, duration: GrantDuration::RobrixSession, .. } if scope == space));
        prompt.update_scope_targets(&mut cx, &[("!target:example.org".into(), Vec::new())]);
        assert!(!prompt.borrow().unwrap().scope_valid);
        prompt.update_scope_targets(&mut cx, &ordinary_info().scope_targets);
        assert!(prompt.borrow().unwrap().scope_valid);
        prompt.set_space_membership_ready(&mut cx, false);
        assert!(!prompt.borrow().unwrap().scope_valid, "wait for trusted space membership before replaying the request");
        prompt.set_space_membership_ready(&mut cx, true);
        assert!(prompt.borrow().unwrap().scope_valid);
        assert_eq!(prompt.borrow().unwrap().scope, Some(space));
        assert_eq!(prompt.borrow().unwrap().durations[prompt.borrow().unwrap().duration], ApprovalDuration::Session);
        picker.set_scope(&mut cx, &RoomScope::Selection { rooms: Vec::new(), spaces: Vec::new() });
        let changed = cx.capture_actions(|cx| cx.widget_action(picker.widget_uid(), PermissionScopeEditorAction::Changed));
        prompt.borrow_mut().unwrap().handle_event(&mut cx, &Event::Actions(changed), &mut Scope::empty());
        assert!(!prompt.borrow().unwrap().scope_valid);
        assert!(matches!(answer(&prompt, &mut cx, 0, false).answer, PermissionPromptAction::AllowOnce));
        assert!(!prompt.borrow().unwrap().view.widget(&cx, ids!(spatial_choice_section)).visible());
        assert!(!prompt.borrow().unwrap().view.widget(&cx, ids!(spatial_picker)).visible());
    }

    #[test]
    fn collection_defaults_all_rooms_but_can_approve_the_current_room() {
        let (mut cx, prompt) = prompt();
        let mut info = ordinary_info();
        info.collection = true;
        info.scope = Some(RoomScope::AllRooms);
        info.scope_targets.clear();
        prompt.show(&mut cx, &info);
        assert_eq!(prompt.borrow().unwrap().spatial_mode, 1);
        prompt.borrow_mut().unwrap().select_spatial_scope(&mut cx, 0);
        assert!(matches!(answer_for_duration(&prompt, &mut cx, ApprovalDuration::Session).answer,
            PermissionPromptAction::AllowScoped { scope, .. } if scope == RoomScope::room("!target:example.org")));
        info.room_id = None;
        prompt.show(&mut cx, &info);
        assert_eq!(prompt.borrow().unwrap().spatial_modes, [1, 2, 3]);
        assert!(matches!(answer_for_duration(&prompt, &mut cx, ApprovalDuration::Session).answer,
            PermissionPromptAction::AllowScoped { scope: RoomScope::AllRooms, .. }));
    }

    #[test]
    fn spatial_flow_answers_include_scope_while_account_requests_hide_the_picker() {
        let (mut cx, prompt) = prompt();
        let mut info = FlowPromptInfo {
            prompt_id: 31, app_name: "Search".into(), app_icon: "🔎".into(), agent: false, action: "Search the current room".into(),
            destination: "Your Matrix server: https://matrix.example.org".into(), sources: vec!["Account data".into()],
            payload: "{\"query\":\"nexus\"}".into(), message_preview: None, write_warning: None, allow_once: true, allow_lasting: true,
            scope: Some(RoomScope::room("!target:example.org")),
            scope_targets: vec![("!target:example.org".into(), vec!["!parent:example.org".into()])],
            room_id: Some("!target:example.org".into()), origin_room_id: None,
        };
        prompt.show_flow(&mut cx, &info);
        assert!(matches!(answer_for_duration(&prompt, &mut cx, ApprovalDuration::Session).answer,
            PermissionPromptAction::AllowFlowScoped { scope, duration: GrantDuration::RobrixSession } if scope == RoomScope::room("!target:example.org")));
        prompt.borrow_mut().unwrap().select_spatial_scope(&mut cx, 1);
        assert!(matches!(answer(&prompt, &mut cx, 2, false).answer,
            PermissionPromptAction::AllowFlowScoped { scope: RoomScope::AllRooms, duration: GrantDuration::Always }));
        assert!(matches!(answer(&prompt, &mut cx, 0, false).answer, PermissionPromptAction::AllowFlowOnce));
        info.scope = None;
        info.scope_targets.clear();
        info.room_id = None;
        prompt.show_flow(&mut cx, &info);
        assert!(!prompt.borrow().unwrap().view.widget(&cx, ids!(spatial_section)).visible());
        assert!(matches!(answer_for_duration(&prompt, &mut cx, ApprovalDuration::Session).answer,
            PermissionPromptAction::AllowFlow { duration: GrantDuration::RobrixSession }));
        let mut account = ordinary_info();
        account.perm = Permission::MatrixProfile;
        account.scope = None;
        account.scope_targets.clear();
        prompt.show(&mut cx, &account);
        assert!(!prompt.borrow().unwrap().view.widget(&cx, ids!(spatial_section)).visible(), "an origin room does not make account permission spatial");
    }
}
