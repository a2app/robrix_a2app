//! Source sharing rules, separate from permission to read or contact a service.
//!
//! Changes go through the runtime so revocation also cancels active agents.

use makepad_widgets::*;
use std::cell::RefCell;
use std::collections::BTreeSet;
use a2app_core::information_flow::{
    self as flow, ActionAuthority, ActionDecision, AuthoritySession, ContextId, ContextSnapshot,
    EffectAuthority, FlowDecision, Influence, ReaderScope, Recipient, SharingDuration, SharingGrant, Source,
};
use a2app_agent::model_transport::ModelRecipient;
use crate::app::ConfirmDeleteAction;
use crate::home::rooms_list::RoomsListRef;
use crate::shared::collapsible_header::CollapsibleHeaderWidgetExt;
use crate::shared::confirmation_modal::ConfirmationModalContent;
use crate::shared::popup_list::{enqueue_popup_notification, PopupKind};
use super::runtime::{with_a2app, A2AppOp};
use super::permission_choices::PermissionChoicesWidgetExt;

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    mod.widgets.SharingCard = set_type_default() do #(SharingCard::register_widget(vm)) {
        ..RoundedView
        width: Fill, height: Fit, flow: Down, spacing: 7, padding: 12
        margin: Inset{bottom: 8}
        draw_bg +: { color: (COLOR_PRIMARY), border_color: (COLOR_DIVIDER), border_size: 1.0, border_radius: 4.0 }
        title := SubsectionLabel { margin: 0 }
        summary := PermissionOptionLabel {}
        open := RobrixNeutralIconButton { text: "View", padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0} }
    }
    let SharingList = FlatList {
        width: Fill, height: Fit, flow: Down
        scroll_bars: ScrollBars { show_scroll_x: false, show_scroll_y: false }
        card := mod.widgets.SharingCard {}
    }
    let TextButton = RobrixNeutralIconButton { padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0} }
    mod.widgets.DataSharing = set_type_default() do #(DataSharing::register_widget(vm)) {
        width: Fill, height: Fill, flow: Down
        tabs := View {
            width: Fill, height: Fit, flow: Down, padding: 15, spacing: 8
            page_choice := PermissionChoices { horizontal: true, tabs: true, labels: ["Rules", "Needs attention", "History"] }
        }
        content := ScrollYView {
            width: Fill, height: Fill, flow: Down, spacing: 12, padding: 15
            rules_page := View {
                width: Fill, height: Fit, flow: Down, spacing: 12
                SubsectionLabel { text: "Saved permissions", margin: 0 }
                PermissionOptionLabel { text: "Apps ask directly when they need permission. You can remove an app's approvals from its regular Permissions panel. This optional advanced view shows sharing rules, action approvals, blocked requests, and previous checks." }
                new_rule := RobrixPositiveIconButton { text: "Add sharing rule", padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0} }
                current_rules := PermissionOptionLabel {}
                sharing_error := PermissionOptionLabel { visible: false, draw_text +: { color: (COLOR_FG_DANGER_RED) } }
                rules_list := SharingList {}
                PermissionOptionLabel { text: "Removing a rule stops future sharing. It cannot recall data already sent." }
            }
            rule_details_page := View {
                visible: false, width: Fill, height: Fit, flow: Down, spacing: 12
                rule_back := TextButton { text: "Back to rules" }
                SubsectionLabel { text: "Saved approval", margin: 0 }
                saved_rule_details := PermissionOptionLabel {}
                saved_identifiers_toggle := RobrixSettingsCollapsibleHeader { title: "Identifiers" }
                saved_identifiers := PermissionOptionLabel { visible: false }
                remove_button := RobrixNegativeIconButton { text: "Remove this permission", padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0} }
                PermissionOptionLabel { text: "Removing this approval stops affected work. Reopen the app or repeat your agent request to continue. Other approvals may still apply; Block this permission in the app's Permissions panel turns the feature off. Your saved data is kept; data already sent cannot be recalled." }
                rule_restore_section := View {
                    visible: false, width: Fill, height: Fit, flow: Down, spacing: 8
                    PermissionOptionLabel { text: "An AI room needs its default rules to read its room and reply. Restoring them keeps your other choices." }
                    rule_restore := TextButton { text: "Restore this AI room's defaults" }
                }
            }
            wizard := View {
                visible: false, width: Fill, height: Fit, flow: Down, spacing: 12
                wizard_progress := PermissionOptionLabel {}
                wizard_title := SubsectionLabel { margin: 0 }
                source_step := View {
                    width: Fill, height: Fit, flow: Down, spacing: 10
                    PermissionOptionLabel { text: "Which data?" }
                    source_choice := PermissionDropDown {}
                    source_default := PermissionOptionLabel {}
                    PermissionOptionLabel { text: "Which mini-app or agent can share it?" }
                    reader_context_section := View {
                        width: Fill, height: Fit, flow: Down, spacing: 6
                        context_choice := PermissionDropDown { labels: ["Choose a mini-app or agent"] }
                    }
                    reader_app_section := View { visible: false, width: Fill, height: Fit, flow: Down, reader_app := PermissionDropDown {} }
                    reader_preview := PermissionOptionLabel {}
                    more_readers := RobrixSettingsCollapsibleHeader { title: "More sharing options" }
                    reader_options := View {
                        visible: false, width: Fill, height: Fit, flow: Down, spacing: 8
                        reader_kind := PermissionChoices { labels: ["Only this mini-app or agent here", "Everywhere this mini-app runs", "All mini-apps and agents"] }
                        PermissionOptionLabel { text: "The broader options include rooms, spaces and account-wide use. Choose them only if you want the same permission to apply everywhere." }
                    }
                    context_details := PermissionOptionLabel { visible: false }
                }
                destination_step := View {
                    visible: false, width: Fill, height: Fit, flow: Down, spacing: 10
                    recipient_kind := PermissionChoices { labels: ["My configured AI service", "A website", "Another room", "The system clipboard", "My Matrix homeserver"] }
                    model_section := View {
                        width: Fill, height: Fit, flow: Down, spacing: 8
                        model_description := PermissionOptionLabel {}
                        PermissionOptionLabel { text: "AI requests may include conversation history and tool results. This rule applies only to the current service, address, model and account credentials." }
                    }
                    network_section := View {
                        visible: false, width: Fill, height: Fit, flow: Down, spacing: 8
                        network_url := RobrixTextInput { width: Fill, empty_text: "https://example.com" }
                        network_preview := PermissionOptionLabel {}
                        PermissionOptionLabel { text: "Every page on this exact site may receive the data. Subdomains, other ports and redirects to other sites need separate rules." }
                    }
                    room_section := View {
                        visible: false, width: Fill, height: Fit, flow: Down, spacing: 8
                        target_room := PermissionDropDown {}
                        PermissionOptionLabel { text: "Data may be included in messages and other operations in this room. Its read and write permissions still apply." }
                    }
                    clipboard_section := View {
                        visible: false, width: Fill, height: Fit, flow: Down
                        PermissionOptionLabel { text: "Other apps on this device may read data copied to the system clipboard." }
                    }
                    homeserver_section := View {
                        visible: false, width: Fill, height: Fit, flow: Down, spacing: 8
                        homeserver_description := PermissionOptionLabel {}
                        PermissionOptionLabel { text: "AI room replies and activity cards are stored there unencrypted." }
                    }
                }
                review_step := View {
                    visible: false, width: Fill, height: Fit, flow: Down, spacing: 10
                    PermissionOptionLabel { text: "How long should this permission last?" }
                    sharing_lifetime := PermissionChoices { labels: ["Until Robrix closes", "Until a room closes", "Until I remove this rule"] }
                    lifetime_room_section := View {
                        visible: false, width: Fill, height: Fit, flow: Down, spacing: 6
                        PermissionOptionLabel { text: "End this rule when I close:" }
                        lifetime_room := PermissionDropDown {}
                    }
                    rule_preview := PermissionOptionLabel {}
                    rule_identifiers_toggle := RobrixSettingsCollapsibleHeader { title: "Identifiers" }
                    rule_identifiers := PermissionOptionLabel { visible: false }
                    PermissionOptionLabel { text: "Allowing sharing sends private room or account data to the destination shown. If several data sources are used together, each must allow that destination." }
                }
                wizard_error := PermissionOptionLabel { visible: false, draw_text +: { color: (COLOR_FG_DANGER_RED) } }
                View {
                    width: Fill, height: Fit, flow: Flow.Right{wrap: true}, spacing: 8
                    wizard_back := TextButton { text: "Back" }
                    wizard_cancel := TextButton { text: "Cancel" }
                    wizard_next := RobrixPositiveIconButton { text: "Continue", padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0} }
                    add_button := RobrixPositiveIconButton { visible: false, text: "Allow sharing", padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0} }
                }
            }
            review_page := View {
                visible: false, width: Fill, height: Fit, flow: Down, spacing: 12
                attention_overview := View {
                    width: Fill, height: Fit, flow: Down, spacing: 12
                    SubsectionLabel { text: "Requests that need a decision", margin: 0 }
                    attention_summary := PermissionOptionLabel {}
                    attention_list := SharingList {}
                    attention_more := RobrixSettingsCollapsibleHeader { visible: false, title: "More requests" }
                    attention_more_section := View {
                        visible: false, width: Fill, height: Fit, flow: Down
                        attention_more_list := SharingList {}
                    }
                }
                attention_details := View {
                    visible: false, width: Fill, height: Fit, flow: Down, spacing: 12
                    attention_back := TextButton { text: "Back to requests" }
                    sharing_details := View {
                        visible: false, width: Fill, height: Fit, flow: Down, spacing: 12
                        SubsectionLabel { text: "Sharing was blocked", margin: 0 }
                        decision_details := PermissionOptionLabel {}
                        decision_identifiers_toggle := RobrixSettingsCollapsibleHeader { title: "Identifiers" }
                        decision_identifiers := PermissionOptionLabel { visible: false }
                        review_section := View {
                            width: Fill, height: Fit, flow: Down, spacing: 8
                            blocked_source_prompt := PermissionOptionLabel { text: "Which data source do you want to review?" }
                            blocked_source_choice := PermissionChoices {}
                            review_button := TextButton { text: "Review sharing rule" }
                            review_problem := PermissionOptionLabel { visible: false }
                        }
                        allow_all_section := View {
                            visible: false, width: Fill, height: Fit, flow: Down, spacing: 8
                            PermissionOptionLabel { text: "Or allow all listed data for this destination. How long should these rules last?" }
                            allow_all_lifetime := PermissionChoices { labels: ["Until Robrix closes", "Until its room closes", "Until I remove these rules"] }
                            allow_all_preview := PermissionOptionLabel {}
                            allow_all_button := RobrixPositiveIconButton { text: "Allow all listed data", padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0} }
                        }
                        decision_restore_section := View {
                            visible: false, width: Fill, height: Fit, flow: Down, spacing: 8
                            decision_restore_text := PermissionOptionLabel {}
                            decision_restore := TextButton { text: "Restore this AI room's defaults" }
                        }
                    }
                    action_review := View {
                        visible: false, width: Fill, height: Fit, flow: Down, spacing: 12
                        SubsectionLabel { text: "Review this exact action", margin: 0 }
                        action_details := PermissionOptionLabel {}
                        action_session := PermissionChoices { labels: ["This exact action once", "Until this app or agent's room closes", "Until Robrix closes"] }
                        authority_button := RobrixPositiveIconButton { padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}, text: "Allow this exact action once" }
                        PermissionOptionLabel { text: "Once permits one unchanged retry. Session approval permits repeated actions to the same target, including different contents. Approval does not run the action automatically." }
                    }
                }
            }
            activity_page := View {
                visible: false, width: Fill, height: Fit, flow: Down, spacing: 12
                history_overview := View {
                    width: Fill, height: Fit, flow: Down, spacing: 10
                    SubsectionLabel { text: "Recent protection history", margin: 0 }
                    PermissionOptionLabel { text: "Saved on this device without message contents, request bodies or credentials." }
                    history_filter_toggle := RobrixSettingsCollapsibleHeader { title: "Filter history" }
                    history_filters := View {
                        visible: false, width: Fill, height: Fit, flow: Down
                        activity_filter := PermissionChoices { labels: ["All activity", "Blocked requests", "Network and room operations", "Settings changes"] }
                    }
                    activity_summary := PermissionOptionLabel {}
                    activity_list := SharingList {}
                    history_more := RobrixSettingsCollapsibleHeader { visible: false, title: "More history" }
                    history_more_section := View {
                        visible: false, width: Fill, height: Fit, flow: Down
                        history_more_list := SharingList {}
                    }
                }
                history_details := View {
                    visible: false, width: Fill, height: Fit, flow: Down, spacing: 12
                    history_back := TextButton { text: "Back to history" }
                    activity_details := PermissionOptionLabel {}
                    history_review := TextButton { visible: false, text: "Review in Needs attention" }
                    PermissionOptionLabel { text: "An allowed check does not prove data was sent. Failed or interrupted requests may already have sent data. Older records eventually expire." }
                }
                activity_warning := PermissionOptionLabel { visible: false, draw_text +: { color: (COLOR_FG_DANGER_RED) } }
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
enum SharingCardTarget {
    Rule(u64),
    Authority(u64),
    EffectAuthority(u64),
    Decision(FlowDecision),
    Action(ActionDecision),
    Activity(u64),
    #[default]
    None,
}

#[derive(Clone, Debug)]
struct SharingCardAction {
    owner: WidgetUid,
    account: String,
    target: SharingCardTarget,
}

#[derive(Script, ScriptHook, Widget)]
pub struct SharingCard {
    #[deref] view: View,
    #[rust] owner: WidgetUid,
    #[rust] account: String,
    #[rust] target: SharingCardTarget,
}

impl Widget for SharingCard {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        if let Event::Actions(actions) = event && self.view.button(cx, ids!(open)).clicked(actions) {
            cx.action(SharingCardAction { owner: self.owner, account: self.account.clone(), target: self.target.clone() });
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)
    }
}

#[derive(Script, ScriptHook, Widget)]
pub struct DataSharing {
    #[deref] view: View,
    #[rust] account: String,
    #[rust] wizard_open: bool,
    #[rust] wizard_step: usize,
    #[rust] wizard_return_page: usize,
    #[rust] reader_options_open: bool,
    #[rust] rule_identifiers_open: bool,
    #[rust] saved_identifiers_open: bool,
    #[rust] decision_identifiers_open: bool,
    #[rust] history_filters_open: bool,
    #[rust] saved_selection: SharingCardTarget,
    #[rust] decision_selection: Option<DecisionKey>,
    #[rust] action_selection: usize,
    #[rust] activity_selection: Option<u64>,
    #[rust] attention_expanded: bool,
    #[rust] history_expanded: bool,
    #[rust] disclosures_initialized: bool,
    #[rust] sources: Vec<(Source, String)>,
    #[rust] rooms: Vec<(String, String)>,
    #[rust] model: Option<ModelRecipient>,
    #[rust] model_error: String,
    #[rust] homeserver: Option<Recipient>,
    /// Sharing rules held by an agent's current turn; they end when it replies.
    #[rust] turn_rules: BTreeSet<u64>,
    #[rust] apps: Vec<(String, String)>,
    #[rust] snapshots: Vec<ContextSnapshot>,
    #[rust] grants: Vec<SharingGrant>,
    #[rust] decisions: Vec<FlowDecision>,
    /// Blocked requests reopened from History, listed until they are resolved.
    #[rust] recalled_decisions: Vec<FlowDecision>,
    /// Open requests that stopped blocking only because their context no longer holds the data.
    #[rust] no_longer_held: BTreeSet<DecisionKey>,
    #[rust] blocked_sources: Vec<Source>,
    #[rust] action_decisions: Vec<ActionDecision>,
    #[rust] authorities: Vec<ActionAuthority>,
    #[rust] effect_authorities: Vec<EffectAuthority>,
    #[rust] activities: Vec<a2app_core::protection_audit::Activity>,
    #[rust] activity_key: Option<(u64, usize, Option<ContextId>, Option<Source>, Option<Recipient>)>,
}

impl Widget for DataSharing {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if self.refresh_account(cx) { return; }
        self.view.handle_event(cx, event, scope);
        let Event::Actions(actions) = event else { return };
        let page = self.view.permission_choices(cx, ids!(page_choice)).selected_item();
        if !self.wizard_open && self.view.permission_choices(cx, ids!(page_choice)).changed(actions).is_some() {
            self.decision_selection = None;
            self.action_selection = 0;
            self.activity_selection = None;
            self.saved_selection = SharingCardTarget::None;
            self.update_page(cx);
            return;
        }
        if page == 1 && !self.wizard_open && self.decision_selection.is_none() && self.action_selection == 0
            && self.view.widget(cx, ids!(attention_more)).visible()
            && let Some(expanded) = self.view.collapsible_header(cx, ids!(attention_more)).expansion_changed(actions)
        {
            self.attention_expanded = expanded;
            self.view.collapsible_header(cx, ids!(attention_more)).set_expanded(cx, expanded, Animate::No);
            self.view.widget(cx, ids!(attention_more_section)).set_visible(cx, expanded);
            self.view.redraw(cx);
            return;
        }
        if page == 2 && !self.wizard_open && self.activity_selection.is_none()
            && self.view.widget(cx, ids!(history_more)).visible()
            && let Some(expanded) = self.view.collapsible_header(cx, ids!(history_more)).expansion_changed(actions)
        {
            self.history_expanded = expanded;
            self.view.collapsible_header(cx, ids!(history_more)).set_expanded(cx, expanded, Animate::No);
            self.view.widget(cx, ids!(history_more_section)).set_visible(cx, expanded);
            self.view.redraw(cx);
            return;
        }
        for action in actions {
            if let Some(action) = action.downcast_ref::<SharingCardAction>()
                && action.owner == self.widget_uid() && action.account == self.account && !self.wizard_open
                && self.card_is_visible(&action.target)
                && match &action.target {
                    SharingCardTarget::Rule(_) | SharingCardTarget::Authority(_) | SharingCardTarget::EffectAuthority(_) => page == 0 && matches!(self.saved_selection, SharingCardTarget::None),
                    SharingCardTarget::Decision(_) | SharingCardTarget::Action(_) => page == 1 && self.decision_selection.is_none() && self.action_selection == 0,
                    SharingCardTarget::Activity(_) => page == 2 && self.activity_selection.is_none(),
                    SharingCardTarget::None => false,
                }
            {
                self.open_card(cx, &action.target);
                return;
            }
        }
        if page == 0 && !self.wizard_open && matches!(self.saved_selection, SharingCardTarget::None)
            && self.view.button(cx, ids!(new_rule)).clicked(actions)
        {
            self.begin_rule(cx);
            return;
        }
        if self.wizard_open && self.view.button(cx, ids!(wizard_cancel)).clicked(actions) {
            self.wizard_open = false;
            self.view.permission_choices(cx, ids!(page_choice)).set_selected_item(cx, self.wizard_return_page);
            self.update_page(cx);
            return;
        }
        if self.wizard_open && self.wizard_step > 0 && self.view.button(cx, ids!(wizard_back)).clicked(actions) {
            self.wizard_step = self.wizard_step.saturating_sub(1);
            self.update_page(cx);
            return;
        }
        if self.wizard_open && self.wizard_step < 2
            && self.view.button(cx, ids!(wizard_next)).clicked(actions) && self.wizard_step_valid(cx).is_ok()
        {
            self.wizard_step = (self.wizard_step + 1).min(2);
            // The AI service can change while the pane is open.
            if self.wizard_step == 1 { self.refresh_model(cx); }
            self.update_page(cx);
            return;
        }
        if self.wizard_open && self.wizard_step == 0
            && let Some(expanded) = self.view.collapsible_header(cx, ids!(more_readers)).expansion_changed(actions)
        {
            self.reader_options_open = expanded;
            self.view.collapsible_header(cx, ids!(more_readers)).set_expanded(cx, expanded, Animate::No);
            self.update_recipient_form(cx);
        }
        if self.wizard_open && self.wizard_step == 2 && self.view.widget(cx, ids!(rule_identifiers_toggle)).visible()
            && let Some(expanded) = self.view.collapsible_header(cx, ids!(rule_identifiers_toggle)).expansion_changed(actions)
        {
            self.rule_identifiers_open = expanded;
            self.view.collapsible_header(cx, ids!(rule_identifiers_toggle)).set_expanded(cx, expanded, Animate::No);
            self.update_recipient_form(cx);
        }
        if page == 0 && !self.wizard_open && !matches!(self.saved_selection, SharingCardTarget::None)
            && self.view.widget(cx, ids!(saved_identifiers_toggle)).visible()
            && let Some(expanded) = self.view.collapsible_header(cx, ids!(saved_identifiers_toggle)).expansion_changed(actions)
        {
            self.saved_identifiers_open = expanded;
            self.view.collapsible_header(cx, ids!(saved_identifiers_toggle)).set_expanded(cx, expanded, Animate::No);
            self.update_saved_details(cx);
        }
        if page == 0 && !self.wizard_open && self.view.button(cx, ids!(rule_back)).clicked(actions) {
            self.saved_selection = SharingCardTarget::None;
            self.update_page(cx);
            return;
        }
        if page == 1 && !self.wizard_open && self.view.button(cx, ids!(attention_back)).clicked(actions) {
            self.decision_selection = None;
            self.action_selection = 0;
            self.update_page(cx);
            return;
        }
        if page == 1 && !self.wizard_open && self.decision_selection.is_some()
            && self.view.widget(cx, ids!(decision_identifiers_toggle)).visible()
            && let Some(expanded) = self.view.collapsible_header(cx, ids!(decision_identifiers_toggle)).expansion_changed(actions)
        {
            self.decision_identifiers_open = expanded;
            self.view.collapsible_header(cx, ids!(decision_identifiers_toggle)).set_expanded(cx, expanded, Animate::No);
            self.update_diagnostic_details(cx);
        }
        if page == 2 && !self.wizard_open && self.view.button(cx, ids!(history_back)).clicked(actions) {
            self.activity_selection = None;
            self.update_page(cx);
            return;
        }
        if page == 2 && !self.wizard_open && self.view.button(cx, ids!(history_review)).clicked(actions)
            && let Some(request) = self.selected_history_request()
        {
            self.review_history_request(cx, request);
            return;
        }
        if page == 2 && !self.wizard_open && self.activity_selection.is_none()
            && let Some(expanded) = self.view.collapsible_header(cx, ids!(history_filter_toggle)).expansion_changed(actions)
        {
            self.history_filters_open = expanded;
            self.view.collapsible_header(cx, ids!(history_filter_toggle)).set_expanded(cx, expanded, Animate::No);
            self.view.widget(cx, ids!(history_filters)).set_visible(cx, self.history_filters_open);
        }
        if page == 2 && !self.wizard_open && self.activity_selection.is_none() && self.history_filters_open
            && self.view.permission_choices(cx, ids!(activity_filter)).changed(actions).is_some()
        {
            self.activity_selection = None;
            self.history_expanded = false;
            self.view.collapsible_header(cx, ids!(history_more)).set_expanded(cx, false, Animate::No);
            self.view.widget(cx, ids!(history_more_section)).set_visible(cx, false);
        }
        if self.wizard_open && self.view.permission_choices(cx, ids!(recipient_kind)).changed(actions) == Some(0) {
            self.refresh_model(cx);
        }
        if self.view.drop_down2(cx, ids!(source_choice)).changed(actions).is_some()
            || self.view.drop_down2(cx, ids!(context_choice)).changed(actions).is_some()
            || self.view.drop_down2(cx, ids!(reader_app)).changed(actions).is_some()
            || self.view.drop_down2(cx, ids!(target_room)).changed(actions).is_some()
            || self.view.drop_down2(cx, ids!(lifetime_room)).changed(actions).is_some()
            || self.view.text_input(cx, ids!(network_url)).changed(actions).is_some()
            || self.view.permission_choices(cx, ids!(reader_kind)).changed(actions).is_some()
            || self.view.permission_choices(cx, ids!(recipient_kind)).changed(actions).is_some()
            || self.view.permission_choices(cx, ids!(sharing_lifetime)).changed(actions).is_some()
            || self.view.permission_choices(cx, ids!(action_session)).changed(actions).is_some()
            || self.view.permission_choices(cx, ids!(blocked_source_choice)).changed(actions).is_some()
            || self.view.permission_choices(cx, ids!(allow_all_lifetime)).changed(actions).is_some()
        {
            self.update_diagnostic_details(cx);
            self.update_recipient_form(cx);
        }
        if page == 1 && !self.wizard_open && self.decision_selection.is_some() && self.view.button(cx, ids!(review_button)).clicked(actions) {
            if let Err(error) = self.review_decision(cx) { self.show_error(error); }
            return;
        }
        if page == 1 && !self.wizard_open && self.decision_selection.is_some() && self.view.button(cx, ids!(allow_all_button)).clicked(actions) {
            self.refresh_model(cx);
            match self.allow_all_grants(cx) {
                Ok(grants) => for grant in grants { cx.action(grant); },
                Err(error) => self.show_error(error),
            }
            self.view.redraw(cx);
            return;
        }
        #[cfg(unix)]
        {
            let room = match page {
                0 if self.view.button(cx, ids!(rule_restore)).clicked(actions) => self.saved_rule_agent_room(),
                1 if self.view.button(cx, ids!(decision_restore)).clicked(actions) => self.selected_decision(cx).and_then(|decision| self.decision_agent_room(decision)),
                _ => None,
            };
            if !self.wizard_open && let Some(room) = room.and_then(|room| matrix_sdk::ruma::OwnedRoomId::try_from(room).ok()) {
                cx.action(A2AppOp::RestoreAgentDefaults(room));
                return;
            }
        }
        if self.view.button(cx, ids!(add_button)).clicked(actions) && self.wizard_open && self.wizard_step == 2 {
            let result = self.sharing_action(cx);
            self.submit(cx, result);
        }
        if page == 0 && !self.wizard_open && self.view.button(cx, ids!(remove_button)).clicked(actions) {
            if let SharingCardTarget::Rule(id) = self.saved_selection
                && let Some((title, body)) = self.grants.iter().find(|grant| grant.id == id).and_then(|grant| self.removal_warning(grant))
            {
                let account = self.account.clone();
                cx.action(ConfirmDeleteAction::Show(RefCell::new(Some(ConfirmationModalContent {
                    title_text: title.into(),
                    body_text: body.into(),
                    accept_button_text: Some("Remove rule".into()),
                    on_accept_clicked: Some(Box::new(move |cx| {
                        if super::information_flow::account().is_ok_and(|current| current == account) {
                            cx.action(A2AppOp::RevokeFlowSharing(id));
                        }
                    })),
                    ..Default::default()
                }))));
                return;
            }
            let result = match self.saved_selection {
                SharingCardTarget::Rule(id) if self.grants.iter().any(|grant| grant.id == id) => Ok(A2AppOp::RevokeFlowSharing(id)),
                SharingCardTarget::Authority(id) if self.authorities.iter().any(|grant| grant.id == id) => Ok(A2AppOp::RevokeFlowAuthority(id)),
                SharingCardTarget::EffectAuthority(id) if self.effect_authorities.iter().any(|grant| grant.id == id) => Ok(A2AppOp::RevokeEffectAuthority(id)),
                _ => Err("This permission is no longer available. Return to Rules to choose another.".into()),
            };
            self.submit(cx, result);
        }
        if page == 1 && !self.wizard_open && self.action_selection != 0 && self.view.button(cx, ids!(authority_button)).clicked(actions) {
            let result = self.authority_action(cx);
            self.submit(cx, result);
        }
        self.view.redraw(cx);
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.refresh_account(cx);
        if !self.disclosures_initialized { self.sync_disclosures(cx); }
        self.refresh_diagnostics(cx);
        self.refresh_rules(cx);
        self.refresh_activity(cx);
        self.update_recipient_form(cx);
        self.update_saved_details(cx);
        let lists = [
            (ids!(rules_list), 0, false), (ids!(attention_list), 1, false),
            (ids!(attention_more_list), 1, true), (ids!(activity_list), 2, false),
            (ids!(history_more_list), 2, true),
        ].map(|(path, kind, additional)| (self.view.widget(cx, path).widget_uid(), kind, additional));
        while let Some(subview) = self.view.draw_walk(cx, scope, walk).step() {
            let uid = subview.widget_uid();
            if let Some(mut list) = subview.as_flat_list().borrow_mut()
                && let Some((_, kind, additional)) = lists.iter().find(|(candidate, _, _)| *candidate == uid)
            {
                self.draw_cards(cx, &mut list, *kind, *additional);
            }
        }
        DrawStep::done()
    }
}

impl DataSharing {
    fn card_is_visible(&self, target: &SharingCardTarget) -> bool {
        match target {
            SharingCardTarget::Decision(decision) => self.decisions.iter().filter(|current| !current.allowed)
                .position(|current| is_decision_for(current, &decision_key(decision))).is_some_and(|index| index < 20 || self.attention_expanded),
            SharingCardTarget::Action(decision) => self.action_decisions.iter().position(|current| current == decision)
                .is_some_and(|index| self.attention_expanded
                    || self.decisions.iter().filter(|current| !current.allowed).count() + index < 20),
            SharingCardTarget::Activity(id) => self.activities.iter().position(|current| current.id == *id)
                .is_some_and(|index| index < 20 || self.history_expanded),
            SharingCardTarget::None => false,
            _ => true,
        }
    }

    fn sync_disclosures(&mut self, cx: &mut Cx) {
        for (path, expanded) in [
            (ids!(more_readers), self.reader_options_open),
            (ids!(rule_identifiers_toggle), self.rule_identifiers_open),
            (ids!(saved_identifiers_toggle), self.saved_identifiers_open),
            (ids!(decision_identifiers_toggle), self.decision_identifiers_open),
            (ids!(history_filter_toggle), self.history_filters_open),
            (ids!(attention_more), self.attention_expanded),
            (ids!(history_more), self.history_expanded),
        ] {
            self.view.collapsible_header(cx, path).set_expanded(cx, expanded, Animate::No);
        }
        self.view.widget(cx, ids!(history_filters)).set_visible(cx, self.history_filters_open);
        self.disclosures_initialized = true;
    }

    fn back(&mut self, cx: &mut Cx) -> bool {
        if self.wizard_open {
            if self.wizard_step > 0 {
                self.wizard_step -= 1;
            } else {
                self.wizard_open = false;
                self.view.permission_choices(cx, ids!(page_choice)).set_selected_item(cx, self.wizard_return_page);
            }
        } else {
            match self.view.permission_choices(cx, ids!(page_choice)).selected_item() {
                0 if !matches!(self.saved_selection, SharingCardTarget::None) => self.saved_selection = SharingCardTarget::None,
                1 if self.decision_selection.is_some() || self.action_selection != 0 => {
                    self.decision_selection = None;
                    self.action_selection = 0;
                }
                2 if self.activity_selection.is_some() => self.activity_selection = None,
                _ => return false,
            }
        }
        self.update_page(cx);
        true
    }

    fn refresh_account(&mut self, cx: &mut Cx) -> bool {
        if self.account == super::information_flow::account().unwrap_or_default() { return false; }
        self.configure(cx);
        true
    }

    fn update_page(&mut self, cx: &mut Cx) {
        self.sync_disclosures(cx);
        let page = self.view.permission_choices(cx, ids!(page_choice)).selected_item();
        let saved = !matches!(self.saved_selection, SharingCardTarget::None);
        self.view.widget(cx, ids!(tabs)).set_visible(cx, !self.wizard_open);
        self.view.widget(cx, ids!(rules_page)).set_visible(cx, page == 0 && !saved && !self.wizard_open);
        self.view.widget(cx, ids!(rule_details_page)).set_visible(cx, page == 0 && saved && !self.wizard_open);
        self.view.widget(cx, ids!(wizard)).set_visible(cx, self.wizard_open);
        self.view.widget(cx, ids!(review_page)).set_visible(cx, page == 1 && !self.wizard_open);
        self.view.widget(cx, ids!(activity_page)).set_visible(cx, page == 2 && !self.wizard_open);
        let reviewing = self.decision_selection.is_some() || self.action_selection != 0;
        self.view.widget(cx, ids!(attention_overview)).set_visible(cx, !reviewing);
        self.view.widget(cx, ids!(attention_details)).set_visible(cx, reviewing);
        self.view.widget(cx, ids!(sharing_details)).set_visible(cx, self.decision_selection.is_some());
        self.view.widget(cx, ids!(action_review)).set_visible(cx, self.action_selection != 0);
        self.view.widget(cx, ids!(history_overview)).set_visible(cx, self.activity_selection.is_none());
        self.view.widget(cx, ids!(history_details)).set_visible(cx, self.activity_selection.is_some());
        for (step, path) in [ids!(source_step), ids!(destination_step), ids!(review_step)].into_iter().enumerate() {
            self.view.widget(cx, path).set_visible(cx, step == self.wizard_step);
        }
        self.view.label(cx, ids!(wizard_progress)).set_text(cx, &format!("Add sharing rule · Step {} of 3", self.wizard_step + 1));
        self.view.label(cx, ids!(wizard_title)).set_text(cx, match self.wizard_step {
            0 => "Choose the data and who can share it",
            1 => "Choose where the data can go",
            _ => "Review and allow sharing",
        });
        self.view.widget(cx, ids!(wizard_back)).set_visible(cx, self.wizard_step > 0);
        self.view.widget(cx, ids!(wizard_next)).set_visible(cx, self.wizard_step < 2);
        self.view.widget(cx, ids!(add_button)).set_visible(cx, self.wizard_step == 2);
        self.update_recipient_form(cx);
        self.view.view(cx, ids!(content)).set_scroll_pos(cx, Vec2d::default());
        self.view.redraw(cx);
    }

    fn begin_rule(&mut self, cx: &mut Cx) {
        self.wizard_return_page = self.view.permission_choices(cx, ids!(page_choice)).selected_item();
        self.wizard_open = true;
        self.wizard_step = 0;
        self.reader_options_open = false;
        self.rule_identifiers_open = false;
        self.view.permission_choices(cx, ids!(reader_kind)).set_selected_item(cx, 0);
        self.view.permission_choices(cx, ids!(sharing_lifetime)).set_selected_item(cx, 0);
        self.update_page(cx);
    }

    fn review_context(&mut self, cx: &mut Cx, context: &ContextId) -> bool {
        self.configure(cx);
        if context.account() != self.account { return false; }
        self.wizard_open = false;
        self.saved_selection = SharingCardTarget::None;
        self.decision_selection = None;
        self.action_selection = 0;
        self.view.permission_choices(cx, ids!(page_choice)).set_selected_item(cx, 1);
        let target = self.action_decisions.iter().find(|decision| &decision.context == context)
            .map(|decision| SharingCardTarget::Action(decision.clone()))
            .or_else(|| self.decisions.iter().find(|decision| &decision.context == context && !decision.allowed)
                .map(|decision| SharingCardTarget::Decision(decision.clone())));
        if let Some(target) = target {
            self.open_card(cx, &target);
            true
        } else {
            self.update_page(cx);
            false
        }
    }

    fn wizard_step_valid(&self, cx: &Cx) -> Result<(), String> {
        match self.wizard_step {
            0 => {
                if self.selected_source(cx)? == Source::UnknownPrivate { return Err("Data with unknown private sources cannot be shared.".into()); }
                self.selected_reader(cx)?;
                Ok(())
            }
            1 => self.selected_recipient(cx).map(|_| ()),
            _ => self.sharing_action(cx).map(|_| ()),
        }
    }

    fn open_card(&mut self, cx: &mut Cx, target: &SharingCardTarget) {
        match target {
            SharingCardTarget::Rule(id) if self.grants.iter().any(|grant| grant.id == *id) => self.saved_selection = target.clone(),
            SharingCardTarget::Authority(id) if self.authorities.iter().any(|grant| grant.id == *id) => self.saved_selection = target.clone(),
            SharingCardTarget::EffectAuthority(id) if self.effect_authorities.iter().any(|grant| grant.id == *id) => self.saved_selection = target.clone(),
            SharingCardTarget::Decision(decision) => {
                let key = decision_key(decision);
                if !self.decisions.iter().any(|current| is_decision_for(current, &key)) { return; }
                self.decision_selection = Some(key);
                self.action_selection = 0;
                self.view.permission_choices(cx, ids!(allow_all_lifetime)).set_selected_item(cx, 0);
                self.refresh_model(cx);
            }
            SharingCardTarget::Action(decision) => {
                let Some(index) = self.action_decisions.iter().position(|current| current == decision) else { return };
                self.action_selection = index + 1;
                self.decision_selection = None;
                self.view.permission_choices(cx, ids!(action_session)).set_selected_item(cx, 0);
            }
            SharingCardTarget::Activity(id) if self.activities.iter().any(|entry| entry.id == *id) => self.activity_selection = Some(*id),
            _ => return,
        }
        self.saved_identifiers_open = false;
        self.decision_identifiers_open = false;
        self.update_diagnostic_details(cx);
        self.update_saved_details(cx);
        self.update_page(cx);
    }

    fn update_saved_details(&mut self, cx: &mut Cx) {
        let details = match self.saved_selection {
            SharingCardTarget::Rule(id) => self.grants.iter().find(|grant| grant.id == id).map(|grant| {
                let is_turn_rule = self.turn_rules.contains(&grant.id);
                let duration = if is_turn_rule { "This turn only. It ends when the agent replies.".into() } else { self.short_duration_label(&grant.duration) };
                (self.sharing_rule_summary(&grant.source, &grant.reader, &grant.recipient, &duration),
                format!("Data: {}\n\nWho can share it: {}\n\nDestination: {}\n\nFor how long: {}{}", self.source_label(&grant.source), self.reader_label(&grant.reader), self.recipient_label(&grant.recipient),
                    if is_turn_rule { "This turn only · " } else { "" }, self.duration_label(&grant.duration)))
            }),
            SharingCardTarget::Authority(id) => self.authorities.iter().find(|grant| grant.id == id).map(|grant| (
                format!("{}\n{}\n{}\n\nAccount: {}", self.context_choice_label(&grant.context), self.action_choice_label(&grant.action), match &grant.session {
                    AuthoritySession::RoomSession { room, .. } => format!("Until {} closes", self.room_name(room)),
                    session => self.authority_duration_label(session),
                }, self.account),
                format!("{}\n\nAction: {}\nTarget: {}\n\n{}", self.context_label(&grant.context), grant.action.kind, grant.action.target, self.authority_duration_label(&grant.session)))),
            SharingCardTarget::EffectAuthority(id) => self.effect_authorities.iter().find(|grant| grant.id == id).map(|grant| {
                let sources = grant.sources.iter().map(|source| self.source_choice_label(source)).collect::<Vec<_>>();
                let destination = grant.recipient.as_ref().map(|recipient| self.recipient_choice_label(recipient)).unwrap_or_else(|| "Inside Robrix".into());
                let scope = match grant.room_scope.as_ref() {
                    Some(a2app_core::permissions::RoomScope::AllRooms) => "\nRooms: All rooms. Blocked rooms stay protected.".into(),
                    Some(a2app_core::permissions::RoomScope::Selection { rooms, spaces }) => format!("\nRooms: {}", rooms.iter().map(|room| self.room_name(room).to_string())
                        .chain(spaces.iter().map(|space| format!("{} and its rooms", self.room_name(space)))).collect::<Vec<_>>().join(", ")),
                    None => String::new(),
                };
                let scope_note = if grant.room_scope.is_some() {
                    "Only this operation in the approved rooms is allowed. Data from other rooms or new sources still needs approval."
                } else { "Only this operation and destination are approved. New data sources require another approval." };
                let summary = format!("{}\n{}\n\nDestination: {destination}\nFor how long: {}{scope}\n\n{}\n\n{scope_note}",
                    self.context_choice_label(&grant.context), self.effect_operation_label(grant), self.effect_duration_label(&grant.duration),
                    if sources.is_empty() { "No private data was included in this approval.".into() } else { format!("Approved data:\n{}", bullets(&sources)) });
                let exact_sources = grant.sources.iter().map(|source| self.source_label(source)).collect::<Vec<_>>();
                let exact_influences = grant.influences.iter().map(|influence| self.influence_label(influence)).collect::<Vec<_>>();
                (summary, format!("{}\n\nOperation: {}\nDestination: {}\n\nData:\n{}\n\nInput sources:\n{}",
                    self.context_label(&grant.context), grant.operation,
                    grant.recipient.as_ref().map(|recipient| self.recipient_label(recipient)).unwrap_or_else(|| "Inside Robrix".into()),
                    bullets(&exact_sources), bullets(&exact_influences)))
            }),
            _ => None,
        };
        self.view.label(cx, ids!(saved_rule_details)).set_text(cx, details.as_ref().map(|(summary, _)| summary.as_str()).unwrap_or("This permission is no longer saved."));
        self.view.label(cx, ids!(saved_identifiers)).set_text(cx, details.as_ref().map(|(_, exact)| exact.as_str()).unwrap_or_default());
        self.view.widget(cx, ids!(saved_identifiers_toggle)).set_visible(cx, details.is_some());
        self.view.widget(cx, ids!(saved_identifiers)).set_visible(cx, details.is_some() && self.saved_identifiers_open);
        self.view.button(cx, ids!(remove_button)).set_enabled(cx, details.is_some());
        self.view.widget(cx, ids!(rule_restore_section)).set_visible(cx, cfg!(unix) && self.saved_rule_agent_room().is_some());
    }

    fn sharing_rule_summary(&self, source: &Source, reader: &ReaderScope, recipient: &Recipient, duration: &str) -> String {
        let destination = match recipient {
            Recipient::MatrixRoom { .. } => self.recipient_choice_label(recipient),
            _ => self.recipient_label(recipient),
        };
        format!("{} can share\n{}\n\nWith: {destination}\nFor how long: {duration}\n\nAccount: {}", self.reader_choice_label(reader), self.source_choice_label(source), self.account)
    }

    fn short_duration_label(&self, duration: &SharingDuration) -> String {
        match duration {
            SharingDuration::RoomSession { room, .. } => format!("Until {} closes", self.room_name(room)),
            duration => self.duration_label(duration),
        }
    }

    /// Task rules borrow a room-session duration, so name them for what they are.
    fn grant_duration_label(&self, grant: &SharingGrant) -> String {
        if self.turn_rules.contains(&grant.id) { "This turn only".into() } else { self.duration_label(&grant.duration) }
    }

    /// The AI room whose agent the selected rule belongs to.
    fn saved_rule_agent_room(&self) -> Option<&str> {
        let SharingCardTarget::Rule(id) = self.saved_selection else { return None };
        match &self.grants.iter().find(|grant| grant.id == id)?.reader {
            ReaderScope::Context(ContextId::Agent { account, room }) if account == &self.account => Some(room),
            _ => None,
        }
    }

    /// Whether `source` -> `recipient` is one of the defaults an AI room in `room` gets. An
    /// unknown AI service can't rule out a model rule.
    fn is_agent_default_pair(&self, room: &str, source: &Source, recipient: &Recipient) -> bool {
        let is_default_source = match source {
            Source::Room { account, room: source_room } => account == &self.account && source_room == room,
            Source::RoomDirectory { account } => account == &self.account,
            _ => false,
        };
        is_default_source && match recipient {
            Recipient::MatrixRoom { account, room: target } => account == &self.account && target == room,
            Recipient::NetworkOrigin(_) => self.homeserver.as_ref() == Some(recipient),
            Recipient::ModelProvider(id) => self.model.as_ref().is_none_or(|model| &model.id == id),
            _ => false,
        }
    }

    /// The confirmation shown before removing a rule other contexts or an AI room depend on.
    fn removal_warning(&self, grant: &SharingGrant) -> Option<(&'static str, String)> {
        if grant.reader == ReaderScope::AllReaders {
            return Some(("Remove this shared rule?", "This sharing rule is shared by all mini-apps and agents. Removing it affects all of them and stops their private-data work. Your data is kept; apps can ask again when needed.".into()));
        }
        match &grant.reader {
            ReaderScope::Context(ContextId::Agent { account, room }) if account == &self.account && grant.duration == SharingDuration::Permanent
                && !self.turn_rules.contains(&grant.id) && self.is_agent_default_pair(room, &grant.source, &grant.recipient) =>
                Some(("Remove this AI room default?", format!("{}'s agent needs this rule to read its room and reply. Removing it blocks those replies until you restore the room's defaults.", self.room_name(room)))),
            _ => None,
        }
    }

    fn draw_cards(&mut self, cx: &mut Cx2d, list: &mut FlatList, kind: usize, additional: bool) {
        let mut rows: Vec<(String, String, SharingCardTarget, &str)> = Vec::new();
        match kind {
            0 => {
                rows.extend(self.grants.iter().map(|grant| (
                    format!("{} → {}", self.source_choice_label(&grant.source), self.recipient_choice_label(&grant.recipient)),
                    format!("{}\n{}", self.reader_choice_label(&grant.reader), self.grant_duration_label(grant)), SharingCardTarget::Rule(grant.id), "View rule")));
                rows.extend(self.authorities.iter().map(|grant| (
                    self.action_choice_label(&grant.action), format!("{}\n{}", self.context_choice_label(&grant.context), self.authority_duration_label(&grant.session)),
                    SharingCardTarget::Authority(grant.id), "View approval")));
                rows.extend(self.effect_authorities.iter().map(|grant| (
                    self.effect_operation_label(grant),
                    format!("{}\n{}\n{}", self.context_choice_label(&grant.context),
                        grant.recipient.as_ref().map(|recipient| self.recipient_choice_label(recipient)).unwrap_or_else(|| "Inside Robrix".into()),
                        self.effect_duration_label(&grant.duration)),
                    SharingCardTarget::EffectAuthority(grant.id), "View approval")));
            }
            1 => {
                rows.extend(self.decisions.iter().filter(|decision| !decision.allowed).map(|decision| (
                    format!("Sharing with {}", self.recipient_choice_label(&decision.recipient)),
                    format!("{}\nStill needs a sharing rule. Review the destination and data before allowing sharing.", self.context_choice_label(&decision.context)),
                    SharingCardTarget::Decision(decision.clone()), "Review sharing")));
                rows.extend(self.action_decisions.iter().map(|decision| (
                    self.action_choice_label(&decision.action), format!("{}\nNeeds approval before retrying.", self.context_choice_label(&decision.context)),
                    SharingCardTarget::Action(decision.clone()), "Review action")));
            }
            _ => rows.extend(self.activities.iter().map(|entry| (
                format!("{} · {}", activity::activity_outcome(entry.outcome), activity::activity_kind(entry.kind)),
                format!("{}\n{}", activity::activity_time(entry.timestamp_ms), entry.context.as_ref().map(|context| self.context_choice_label(context)).unwrap_or_else(|| "Account settings".into())),
                SharingCardTarget::Activity(entry.id), "View details"))),
        }
        let (start, limit) = if additional { (20, usize::MAX) } else if kind == 0 { (0, usize::MAX) } else { (0, 20) };
        for (index, (title, summary, target, button)) in rows.into_iter().enumerate().skip(start).take(limit) {
            if let Some(item) = list.item(cx, LiveId::from_str(&format!("sharing-{kind}-{index}")), id!(card)) {
                if let Some(mut row) = item.borrow_mut::<SharingCard>() {
                    row.owner = self.widget_uid();
                    row.account.clone_from(&self.account);
                    row.target = target;
                    row.view.label(cx, ids!(title)).set_text(cx, &title);
                    row.view.label(cx, ids!(summary)).set_text(cx, &summary);
                    row.view.button(cx, ids!(open)).set_text(cx, button);
                }
                item.draw_all(cx, &mut Scope::empty());
            }
        }
        list.items.retain_visible();
    }

    fn configure(&mut self, cx: &mut Cx) {
        self.activity_key = None;
        self.view.permission_choices(cx, ids!(action_session)).set_selected_item(cx, 0);
        let previous_source = self.selected_source(cx).ok();
        let previous_draft = if self.wizard_open { self.sharing_draft(cx) } else { None };
        let previous_app = self.apps.get(self.view.drop_down2(cx, ids!(reader_app)).selected_item()).map(|(id, _)| id.clone());
        let previous_target = self.rooms.get(self.view.drop_down2(cx, ids!(target_room)).selected_item()).map(|(id, _)| id.clone());
        let previous_expiry = self.rooms.get(self.view.drop_down2(cx, ids!(lifetime_room)).selected_item()).map(|(id, _)| id.clone());
        let account = super::information_flow::account().unwrap_or_default();
        let account_changed = self.account != account;
        self.account = account;
        if account_changed {
            self.wizard_open = false;
            self.wizard_step = 0;
            self.reader_options_open = false;
            self.rule_identifiers_open = false;
            self.saved_identifiers_open = false;
            self.decision_identifiers_open = false;
            self.history_filters_open = false;
            self.attention_expanded = false;
            self.history_expanded = false;
            self.saved_selection = SharingCardTarget::None;
            self.decision_selection = None;
            self.recalled_decisions.clear();
            self.action_selection = 0;
            self.activity_selection = None;
            for id in [ids!(reader_kind), ids!(sharing_lifetime), ids!(recipient_kind), ids!(page_choice), ids!(allow_all_lifetime)] {
                self.view.permission_choices(cx, id).set_selected_item(cx, 0);
            }
            self.view.text_input(cx, ids!(network_url)).set_text(cx, "");
        }
        self.rooms = if cx.has_global::<RoomsListRef>() {
            cx.get_global::<RoomsListRef>().permission_targets().into_iter()
                .filter(|(_, _, space)| !space).map(|(id, name, _)| (id, name)).collect()
        } else { Vec::new() };
        self.apps = with_a2app(|state| state.registry.iter().map(|app| (app.id.clone(), app.name.clone())).collect()).unwrap_or_default();
        self.view.drop_down2(cx, ids!(reader_app)).set_labels(cx, self.apps.iter().map(|(_, name)| name.clone()).collect());
        self.view.drop_down2(cx, ids!(reader_app)).set_selected_item(cx,
            previous_app.filter(|_| !account_changed).and_then(|app| self.apps.iter().position(|(id, _)| id == &app)).unwrap_or(0));
        self.view.drop_down2(cx, ids!(lifetime_room)).set_labels(cx, self.rooms.iter().map(|(_, name)| name.clone()).collect());
        self.view.drop_down2(cx, ids!(lifetime_room)).set_selected_item(cx,
            previous_expiry.filter(|_| !account_changed).and_then(|room| self.rooms.iter().position(|(id, _)| id == &room)).unwrap_or(0));
        let previous_source = previous_source.filter(|_| !account_changed);
        let saved = flow::sharing_grants().unwrap_or_default().into_iter().map(|grant| grant.source);
        self.sources = source_options(&self.account, &self.rooms, saved, previous_source.as_ref());
        self.view.drop_down2(cx, ids!(source_choice)).set_labels(cx, self.sources.iter().map(|(source, _)| self.source_choice_label(source)).collect());
        self.view.drop_down2(cx, ids!(source_choice)).set_selected_item(cx,
            previous_source.and_then(|source| self.sources.iter().position(|(candidate, _)| candidate == &source)).unwrap_or(0));
        self.view.drop_down2(cx, ids!(target_room)).set_labels(cx, self.rooms.iter().map(|(_, name)| name.clone()).collect());
        self.view.drop_down2(cx, ids!(target_room)).set_selected_item(cx,
            previous_target.filter(|_| !account_changed).and_then(|room| self.rooms.iter().position(|(id, _)| id == &room)).unwrap_or(0));
        self.refresh_model(cx);
        self.homeserver = homeserver_recipient();
        self.view.label(cx, ids!(homeserver_description)).set_text(cx, &match &self.homeserver {
            Some(Recipient::NetworkOrigin(origin)) => format!("Your account's homeserver: {origin}"),
            _ => "Sign in to Matrix to see your homeserver.".into(),
        });
        self.refresh_diagnostics(cx);
        // A rebuilt list must never change what an open rule would allow; close it instead.
        if previous_draft.is_some() && self.wizard_open && self.sharing_draft(cx) != previous_draft {
            self.wizard_open = false;
            self.view.permission_choices(cx, ids!(page_choice)).set_selected_item(cx, self.wizard_return_page);
            enqueue_popup_notification("The sharing rule you were reviewing changed, so it was closed. Review it again before allowing sharing.", PopupKind::Warning, Some(6.0));
        }
        self.update_recipient_form(cx);
        self.refresh_rules(cx);
        self.update_page(cx);
        self.view.redraw(cx);
    }

    /// Re-reads the configured AI service, which can change while the pane is open.
    fn refresh_model(&mut self, cx: &mut Cx) {
        let prefs = with_a2app(|state| state.agent_prefs.clone()).unwrap_or_else(a2app_agent::prefs::load_agent_prefs);
        match current_model(&prefs) {
            Ok(model) => {
                let location = if model.local { "Local endpoint" } else { "Online service" };
                self.view.label(cx, ids!(model_description)).set_text(cx, &format!("{}\n{location}: {}", model.label, model.endpoint));
                self.model = Some(model);
                self.model_error.clear();
            }
            Err(error) => {
                self.view.label(cx, ids!(model_description)).set_text(cx, &format!("{error}\nConfigure a supported provider in Setup AI Providers."));
                self.model = None;
                self.model_error = error;
            }
        }
    }

    fn sharing_draft(&self, cx: &Cx) -> Option<(Source, Recipient, ReaderScope, SharingDuration)> {
        match self.sharing_action(cx) {
            Ok(A2AppOp::GrantFlowSharing { source, recipient, reader, duration }) => Some((source, recipient, reader, duration)),
            _ => None,
        }
    }

    fn selected_source(&self, cx: &Cx) -> Result<Source, String> {
        self.sources.get(self.view.drop_down2(cx, ids!(source_choice)).selected_item())
            .map(|(source, _)| source.clone()).ok_or_else(|| "Sign in and select a data source.".into())
    }

    fn selected_recipient(&self, cx: &Cx) -> Result<Recipient, String> {
        match self.view.permission_choices(cx, ids!(recipient_kind)).selected_item() {
            0 => self.model.as_ref().map(|model| Recipient::ModelProvider(model.id.clone()))
                .ok_or_else(|| "Configure a supported AI model service first.".into()),
            1 => Recipient::network_origin(self.view.text_input(cx, ids!(network_url)).text().trim()),
            2 => self.rooms.get(self.view.drop_down2(cx, ids!(target_room)).selected_item())
                .map(|(room, _)| Recipient::MatrixRoom { account: self.account.clone(), room: room.clone() })
                .ok_or_else(|| "Select a joined destination room.".into()),
            3 => Ok(Recipient::Clipboard),
            4 => self.homeserver.clone().ok_or_else(|| "Sign in to Matrix to share with your homeserver.".into()),
            _ => Err("Select a recipient type.".into()),
        }
    }

    fn update_recipient_form(&mut self, cx: &mut Cx) {
        let kind = self.view.permission_choices(cx, ids!(recipient_kind)).selected_item();
        self.view.widget(cx, ids!(model_section)).set_visible(cx, kind == 0);
        self.view.widget(cx, ids!(network_section)).set_visible(cx, kind == 1);
        self.view.widget(cx, ids!(room_section)).set_visible(cx, kind == 2);
        self.view.widget(cx, ids!(clipboard_section)).set_visible(cx, kind == 3);
        self.view.widget(cx, ids!(homeserver_section)).set_visible(cx, kind == 4);
        if kind == 1 {
            let preview = match self.selected_recipient(cx) {
                Ok(Recipient::NetworkOrigin(origin)) => format!("Recipient: {origin}"),
                _ => "Enter an HTTP or HTTPS URL without account credentials.".into(),
            };
            self.view.label(cx, ids!(network_preview)).set_text(cx, &preview);
        }
        let reader_kind = self.view.permission_choices(cx, ids!(reader_kind)).selected_item();
        self.view.widget(cx, ids!(reader_app_section)).set_visible(cx, reader_kind == 1);
        self.view.widget(cx, ids!(reader_context_section)).set_visible(cx, reader_kind == 0);
        self.view.widget(cx, ids!(reader_options)).set_visible(cx, self.reader_options_open);
        let room_lifetime = self.view.permission_choices(cx, ids!(sharing_lifetime)).selected_item() == 1;
        self.view.widget(cx, ids!(lifetime_room_section)).set_visible(cx, room_lifetime);
        let preview = self.selected_reader(cx).map(|reader| self.reader_label(&reader))
            .unwrap_or_else(|error| error);
        self.view.label(cx, ids!(reader_preview)).set_text(cx, &preview);
        let (preview, identifiers, can_allow, already_saved) = match self.sharing_action(cx) {
            Ok(A2AppOp::GrantFlowSharing { source, recipient, reader, duration }) => {
                let existing = self.grants.iter().find(|grant| grant.source == source && grant.recipient == recipient
                    && grant.reader == reader && grant.duration == duration);
                // Saving a copy of a turn rule would only return that rule, which ends with the turn.
                let is_turn_rule = existing.is_some_and(|grant| self.turn_rules.contains(&grant.id));
                let saved = existing.is_some() && !is_turn_rule;
                let heading = if saved { "This rule already allows:" } else { "You will allow:" };
                let summary = self.sharing_rule_summary(&source, &reader, &recipient, &self.short_duration_label(&duration));
                (if is_turn_rule {
                    format!("This turn only: the agent's current request holds this rule, and it ends when the agent replies. Choose another duration to keep this sharing.\n\n{summary}")
                } else { format!("{heading}\n{summary}") },
                format!("{heading}\n{}\n\nTo share:\n{}\n\nWith:\n{}\n\nFor how long: {}", self.reader_label(&reader),
                    self.source_label(&source), self.recipient_label(&recipient), self.duration_label(&duration)), !saved && !is_turn_rule, saved)
            }
            Err(error) => (error, String::new(), false, false),
            _ => unreachable!("sharing_action only creates a sharing grant"),
        };
        self.view.label(cx, ids!(rule_preview)).set_text(cx, &preview);
        self.view.label(cx, ids!(rule_identifiers)).set_text(cx, &identifiers);
        self.view.widget(cx, ids!(rule_identifiers_toggle)).set_visible(cx, !identifiers.is_empty());
        self.view.widget(cx, ids!(rule_identifiers)).set_visible(cx, !identifiers.is_empty() && self.rule_identifiers_open);
        self.view.button(cx, ids!(add_button)).set_enabled(cx, can_allow);
        self.view.widget(cx, ids!(add_button)).set_disabled(cx, !can_allow);
        self.view.button(cx, ids!(add_button)).set_text(cx, if already_saved { "Sharing rule saved" } else { "Allow sharing" });
        self.view.button(cx, ids!(wizard_cancel)).set_text(cx, if already_saved && self.wizard_step == 2 { "Done" } else { "Cancel" });
        let step_error = self.wizard_step_valid(cx).err();
        self.view.button(cx, ids!(wizard_next)).set_enabled(cx, step_error.is_none());
        self.view.widget(cx, ids!(wizard_error)).set_visible(cx, self.wizard_open && step_error.is_some());
        self.view.label(cx, ids!(wizard_error)).set_text(cx, step_error.as_deref().unwrap_or_default());
    }

    fn refresh_rules(&mut self, cx: &mut Cx) {
        self.turn_rules = turn_rule_ids();
        match flow::sharing_grants() {
            Ok(grants) => {
                self.grants = grants.into_iter().filter(|grant| match &grant.source {
                    Source::Account { account } | Source::Room { account, .. } | Source::RoomDirectory { account } => account == &self.account,
                    Source::UnknownPrivate => false,
                }).collect();
                self.view.widget(cx, ids!(sharing_error)).set_visible(cx, false);
                self.view.label(cx, ids!(current_rules)).set_text(cx, if self.grants.is_empty() && self.authorities.is_empty() && self.effect_authorities.is_empty() {
                    "No extra sharing rules or action approvals. Private data is protected by default."
                } else { "Your saved sharing rules and action approvals:" });
            }
            Err(error) => {
                self.grants.clear();
                self.view.label(cx, ids!(sharing_error)).set_text(cx, &error);
                self.view.widget(cx, ids!(sharing_error)).set_visible(cx, true);
                self.view.label(cx, ids!(current_rules)).set_text(cx, "Sharing rules are unavailable. Private data remains protected.");
            }
        }
        let source = self.selected_source(cx);
        let hint = match &source {
            Ok(Source::Room { .. }) => "Data may return to its original room by default. Other rooms and services need a rule.",
            Ok(Source::RoomDirectory { .. }) => "Names, topics and member counts of your rooms and spaces. AI rooms share it with your AI service, your homeserver and their own room by default.",
            Ok(Source::UnknownPrivate) => "Data with unknown private sources cannot be shared.",
            _ => "Account data stays in Robrix by default. Choose exactly who can share it and where it can go.",
        };
        self.view.label(cx, ids!(source_default)).set_text(cx, hint);
    }

    fn recipient_label(&self, recipient: &Recipient) -> String {
        match recipient {
            Recipient::NetworkOrigin(origin) if self.homeserver.as_ref() == Some(recipient) => format!("My Matrix homeserver: {origin}"),
            Recipient::NetworkOrigin(origin) => format!("Website: {origin}"),
            Recipient::ModelProvider(id) => match self.model.as_ref().filter(|model| &model.id == id) {
                Some(model) => format!("AI model: {} · {}", model.label, model.endpoint),
                None => format!("Other model service: {id}"),
            },
            Recipient::MatrixRoom { account, room } => {
                let name = self.rooms.iter().find(|(id, _)| id == room).map(|(_, name)| name.as_str()).unwrap_or(room);
                format!("Matrix room: {name} ({room}) · {account}")
            }
            Recipient::External => "Unrestricted external sharing (remove to protect this source)".into(),
            Recipient::Clipboard => "System clipboard · other apps on this device may read it".into(),
        }
    }

    fn show_error(&self, error: String) {
        enqueue_popup_notification(error, PopupKind::Error, Some(7.0));
    }

    fn submit(&mut self, cx: &mut Cx, result: Result<A2AppOp, String>) {
        match result {
            Ok(action) => cx.action(action),
            Err(error) => self.show_error(error),
        }
        self.view.redraw(cx);
    }

}

/// The wizard's data sources: account data, the room and space directory, joined rooms, then
/// sources of saved rules and `keep` (an open rule's source), so neither vanishes after leaving a room.
fn source_options(account: &str, rooms: &[(String, String)], saved: impl IntoIterator<Item = Source>, keep: Option<&Source>) -> Vec<(Source, String)> {
    if account.is_empty() { return Vec::new(); }
    let mut sources = vec![
        (Source::Account { account: account.into() }, format!("Account data · {account}")),
        (Source::RoomDirectory { account: account.into() }, "Room and space directory".into()),
    ];
    sources.extend(rooms.iter().map(|(room, name)| (Source::Room { account: account.into(), room: room.clone() }, format!("Room · {name} ({room})"))));
    for source in saved.into_iter().chain(keep.cloned()) {
        let is_owned = match &source {
            Source::Account { account: owner } | Source::Room { account: owner, .. } | Source::RoomDirectory { account: owner } => owner == account,
            Source::UnknownPrivate => false,
        };
        if is_owned && !sources.iter().any(|(candidate, _)| candidate == &source) {
            let label = if let Source::Room { room, .. } = &source { format!("Earlier room · {room}") } else { String::new() };
            sources.push((source, label));
        }
    }
    sources
}

/// Sharing rules held by AI rooms' current turns; each turn's end revokes its own.
#[cfg(unix)]
fn turn_rule_ids() -> BTreeSet<u64> {
    with_a2app(|state| state.task_ledger.tasks().flat_map(|task| task.grants.iter()).filter_map(|grant| match grant {
        a2app_core::task_grants::GrantRef::Flow(id) => Some(*id),
        _ => None,
    }).collect()).unwrap_or_default()
}

#[cfg(not(unix))]
fn turn_rule_ids() -> BTreeSet<u64> { BTreeSet::new() }

#[cfg(test)]
thread_local! {
    static TEST_MODEL: RefCell<Option<Result<ModelRecipient, String>>> = const { RefCell::new(None) };
    static TEST_HOMESERVER: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn current_model(prefs: &a2app_agent::prefs::AgentPrefs) -> Result<ModelRecipient, String> {
    #[cfg(test)]
    if let Some(model) = TEST_MODEL.with(|model| model.borrow().clone()) { return model; }
    a2app_agent::model_transport::current_recipient(prefs)
}

/// The homeserver origin that AI reply and activity cards are written to.
fn homeserver_recipient() -> Option<Recipient> {
    #[cfg(test)]
    if let Some(url) = TEST_HOMESERVER.with(|url| url.borrow().clone()) { return Recipient::network_origin(&url).ok(); }
    crate::sliding_sync::get_client().and_then(|client| Recipient::network_origin(client.homeserver().as_str()).ok())
}

mod diagnostics;
mod activity;
use diagnostics::{bullets, decision_key, is_decision_for, DecisionKey};

impl DataSharingRef {
    pub fn configure(&self, cx: &mut Cx) {
        if let Some(mut inner) = self.borrow_mut() { inner.configure(cx); }
    }

    pub fn back(&self, cx: &mut Cx) -> bool {
        self.borrow_mut().is_some_and(|mut inner| inner.back(cx))
    }

    pub fn review_context(&self, cx: &mut Cx, context: &ContextId) -> bool {
        self.borrow_mut().is_some_and(|mut inner| inner.review_context(cx, context))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestAccount(Option<String>);

    impl TestAccount {
        fn set(account: &str) -> Self {
            Self(crate::a2app::information_flow::TEST_ACCOUNT.with(|current| current.replace(Some(account.into()))))
        }
    }

    impl Drop for TestAccount {
        fn drop(&mut self) {
            crate::a2app::information_flow::TEST_ACCOUNT.with(|current| current.replace(self.0.take()));
        }
    }

    fn editor() -> (Cx, WidgetRef) {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let widget = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            makepad_code_editor::script_mod(vm);
            crate::shared::script_mod(vm, crate::settings::app_preferences::Appearance::Light);
            super::super::permission_choices::script_mod(vm);
            super::super::permission_message_preview::script_mod(vm);
            super::super::permission_prompt::script_mod(vm);
            super::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.DataSharing {} });
            WidgetRef::script_from_value(vm, value)
        });
        (cx, widget)
    }

    fn expand(editor: &mut DataSharing, cx: &mut Cx, path: &[LiveId], expanded: bool) -> ActionsBuf {
        let uid = editor.view.collapsible_header(cx, path).widget_uid();
        let event = cx.capture_actions(|cx| cx.widget_action(uid,
            crate::shared::collapsible_header::CollapsibleHeaderAction::ExpansionChanged(expanded)));
        cx.capture_actions(|cx| editor.handle_event(cx, &Event::Actions(event), &mut Scope::empty()))
    }

    fn select_first(editor: &mut DataSharing) {
        editor.decision_selection = editor.decisions.first().map(decision_key);
    }

    fn press(editor: &mut DataSharing, cx: &mut Cx, button: &[LiveId]) -> ActionsBuf {
        let uid = editor.view.button(cx, button).widget_uid();
        let event = cx.capture_actions(|cx| cx.widget_action(uid, ButtonAction::Clicked(Default::default())));
        cx.capture_actions(|cx| editor.handle_event(cx, &Event::Actions(event), &mut Scope::empty()))
    }

    fn unique_account(name: &str) -> String {
        format!("@{name}-{}:example.org", std::process::id())
    }

    fn test_model(id: &str) -> ModelRecipient {
        ModelRecipient { id: id.into(), label: format!("Model {id}"), endpoint: "https://model.example".into(), local: false }
    }

    struct TestModel(Option<Result<ModelRecipient, String>>);

    impl TestModel {
        fn set(model: Result<ModelRecipient, String>) -> Self {
            Self(TEST_MODEL.with(|current| current.replace(Some(model))))
        }
    }

    impl Drop for TestModel {
        fn drop(&mut self) {
            TEST_MODEL.with(|current| current.replace(self.0.take()));
        }
    }

    struct TestHomeserver(Option<String>);

    impl TestHomeserver {
        fn set(url: &str) -> Self {
            Self(TEST_HOMESERVER.with(|current| current.replace(Some(url.into()))))
        }
    }

    impl Drop for TestHomeserver {
        fn drop(&mut self) {
            TEST_HOMESERVER.with(|current| current.replace(self.0.take()));
        }
    }

    fn granted(actions: &ActionsBuf) -> Vec<(Source, Recipient, ReaderScope, SharingDuration)> {
        actions.iter().filter_map(|action| match action.downcast_ref::<A2AppOp>() {
            Some(A2AppOp::GrantFlowSharing { source, recipient, reader, duration }) => Some((source.clone(), recipient.clone(), reader.clone(), duration.clone())),
            _ => None,
        }).collect()
    }

    #[test]
    fn sharing_headers_follow_expansion_and_reset_without_granting_permissions() {
        let account = "@alice:example.org";
        let _account = TestAccount::set(account);
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.account = account.into();
        for path in [ids!(more_readers), ids!(rule_identifiers_toggle), ids!(saved_identifiers_toggle),
            ids!(decision_identifiers_toggle), ids!(history_filter_toggle), ids!(attention_more), ids!(history_more)]
        {
            assert!(editor.view.collapsible_header(&cx, path).borrow().is_some());
        }
        editor.begin_rule(&mut cx);
        for expanded in [true, true, false] {
            let actions = expand(&mut editor, &mut cx, ids!(more_readers), expanded);
            assert_eq!(editor.reader_options_open, expanded, "use the header's state rather than invert it");
            assert_eq!(editor.view.widget(&cx, ids!(reader_options)).visible(), expanded);
            assert!(!actions.iter().any(|action| action.downcast_ref::<A2AppOp>().is_some()));
        }
        expand(&mut editor, &mut cx, ids!(more_readers), true);
        editor.begin_rule(&mut cx);
        assert!(!editor.reader_options_open);
        assert!(!editor.view.widget(&cx, ids!(reader_options)).visible());
        editor.wizard_open = false;
        editor.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 2);
        editor.update_page(&mut cx);
        for expanded in [true, true, false] {
            let actions = expand(&mut editor, &mut cx, ids!(history_filter_toggle), expanded);
            assert_eq!(editor.history_filters_open, expanded);
            assert_eq!(editor.view.widget(&cx, ids!(history_filters)).visible(), expanded);
            assert!(!actions.iter().any(|action| action.downcast_ref::<A2AppOp>().is_some()));
        }
        editor.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 0);
        expand(&mut editor, &mut cx, ids!(history_filter_toggle), true);
        assert!(!editor.history_filters_open, "a hidden history header must not open its filters");
        editor.history_filters_open = true;
        editor.attention_expanded = true;
        editor.history_expanded = true;
        let _next_account = TestAccount::set("@sharing-next:example.org");
        editor.configure(&mut cx);
        assert!(!editor.history_filters_open);
        assert!(!editor.attention_expanded);
        assert!(!editor.history_expanded);
        assert!(!editor.view.widget(&cx, ids!(history_filters)).visible());
    }

    #[test]
    fn cached_record_disclosures_keep_first_twenty_and_reject_collapsed_card_actions() {
        let account = "@alice:example.org";
        let _account = TestAccount::set(account);
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.account = account.into();
        let snapshot = context_fixture();
        for index in 0..21 {
            editor.decisions.push(FlowDecision {
                context: snapshot.context.clone(), epoch: snapshot.epoch,
                recipient: Recipient::NetworkOrigin(format!("https://request{index}.example")),
                sources: snapshot.label.clone(), denied_sources: snapshot.label.clone(), allowed: false,
            });
            editor.activities.push(a2app_core::protection_audit::Activity {
                id: index, timestamp_ms: index, account: account.into(), context: Some(snapshot.context.clone()),
                recipient: None, sources: snapshot.label.clone(), blocked_sources: snapshot.label.clone(),
                kind: a2app_core::protection_audit::ActivityKind::SharingCheck,
                outcome: a2app_core::protection_audit::ActivityOutcome::Blocked,
            });
        }
        editor.action_decisions.push(ActionDecision {
            context: snapshot.context, epoch: snapshot.epoch, influences: snapshot.influences,
            action: flow::SensitiveAction { kind: "matrix.message.send".into(), target: "!target:example.org".into() },
            allowed: false, request: None,
        });
        let first = SharingCardTarget::Decision(editor.decisions[19].clone());
        let extra = SharingCardTarget::Decision(editor.decisions[20].clone());
        let action = SharingCardTarget::Action(editor.action_decisions[0].clone());
        let activity = SharingCardTarget::Activity(20);
        assert!(editor.card_is_visible(&first));
        assert!(!editor.card_is_visible(&extra));
        assert!(!editor.card_is_visible(&action));
        assert!(!editor.card_is_visible(&activity));
        editor.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 1);
        editor.view.widget(&cx, ids!(attention_more)).set_visible(&mut cx, true);
        for expanded in [true, true] {
            let actions = expand(&mut editor, &mut cx, ids!(attention_more), expanded);
            assert!(editor.attention_expanded);
            assert!(editor.card_is_visible(&extra));
            assert!(editor.card_is_visible(&action));
            assert!(editor.view.widget(&cx, ids!(attention_more_section)).visible());
            assert!(!actions.iter().any(|action| action.downcast_ref::<A2AppOp>().is_some()));
        }
        let uid = editor.view.collapsible_header(&cx, ids!(attention_more)).widget_uid();
        let stale = SharingCardAction { owner: editor.widget_uid(), account: account.into(), target: extra.clone() };
        let events = cx.capture_actions(|cx| {
            cx.action(stale.clone());
            cx.widget_action(uid, crate::shared::collapsible_header::CollapsibleHeaderAction::ExpansionChanged(false));
        });
        editor.handle_event(&mut cx, &Event::Actions(events), &mut Scope::empty());
        assert!(!editor.attention_expanded);
        assert!(editor.decision_selection.is_none(), "collapse must discard a cached card action in the same batch");
        assert!(editor.card_is_visible(&first));
        assert!(!editor.card_is_visible(&extra));
        let events = cx.capture_actions(|cx| cx.action(stale));
        editor.handle_event(&mut cx, &Event::Actions(events), &mut Scope::empty());
        assert!(editor.decision_selection.is_none(), "an extra row cannot open while its section is hidden");
        editor.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 2);
        editor.view.widget(&cx, ids!(history_more)).set_visible(&mut cx, true);
        expand(&mut editor, &mut cx, ids!(history_more), true);
        assert!(editor.card_is_visible(&activity));
        assert!(editor.view.widget(&cx, ids!(history_more_section)).visible());
        expand(&mut editor, &mut cx, ids!(history_more), false);
        assert!(!editor.card_is_visible(&activity));
        assert!(!editor.view.widget(&cx, ids!(history_more_section)).visible());
    }

    #[test]
    fn editor_preserves_account_and_room_identities_and_canonical_origin() {
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.account = "@alice:example.org".into();
        let source = Source::Room { account: editor.account.clone(), room: "!private:example.org".into() };
        editor.sources.push((source.clone(), "Private room".into()));
        editor.rooms.push(("!target:example.org".into(), "Target".into()));
        assert_eq!(editor.selected_source(&cx).unwrap(), source);
        editor.view.permission_choices(&cx, ids!(recipient_kind)).set_selected_item(&mut cx, 1);
        editor.view.text_input(&cx, ids!(network_url)).set_text(&mut cx, "https://EXAMPLE.com:443/path?query=1");
        assert_eq!(editor.selected_recipient(&cx).unwrap(), Recipient::NetworkOrigin("https://example.com".into()));
        editor.view.permission_choices(&cx, ids!(recipient_kind)).set_selected_item(&mut cx, 2);
        assert_eq!(editor.selected_recipient(&cx).unwrap(), Recipient::MatrixRoom {
            account: "@alice:example.org".into(), room: "!target:example.org".into(),
        });
    }

    #[test]
    fn editor_rejects_missing_recipients_and_does_not_offer_unrestricted_release() {
        let (mut cx, widget) = editor();
        let editor = widget.borrow_mut::<DataSharing>().unwrap();
        assert!(editor.selected_source(&cx).is_err());
        assert!(editor.selected_recipient(&cx).is_err());
        editor.view.permission_choices(&cx, ids!(recipient_kind)).set_selected_item(&mut cx, 1);
        assert!(editor.selected_recipient(&cx).is_err());
        editor.view.text_input(&cx, ids!(network_url)).set_text(&mut cx, "https://user:secret@example.com");
        assert!(editor.selected_recipient(&cx).is_err());
        editor.view.permission_choices(&cx, ids!(recipient_kind)).set_selected_item(&mut cx, 2);
        assert!(editor.selected_recipient(&cx).is_err());
    }

    #[test]
    fn reopening_sharing_resets_action_approval_to_once_and_preserves_other_drafts() {
        let account = "@sharing-reopen:example.org";
        let _account = TestAccount::set(account);
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.account = account.into();
        editor.view.permission_choices(&cx, ids!(action_session)).set_selected_item(&mut cx, 2);
        editor.view.permission_choices(&cx, ids!(reader_kind)).set_selected_item(&mut cx, 2);
        editor.view.permission_choices(&cx, ids!(sharing_lifetime)).set_selected_item(&mut cx, 2);
        editor.view.permission_choices(&cx, ids!(recipient_kind)).set_selected_item(&mut cx, 1);
        editor.view.text_input(&cx, ids!(network_url)).set_text(&mut cx, "https://example.org/draft");
        editor.configure(&mut cx);
        assert_eq!(editor.view.permission_choices(&cx, ids!(action_session)).selected_item(), 0);
        assert_eq!(editor.view.permission_choices(&cx, ids!(reader_kind)).selected_item(), 2);
        assert_eq!(editor.view.permission_choices(&cx, ids!(sharing_lifetime)).selected_item(), 2);
        assert_eq!(editor.view.permission_choices(&cx, ids!(recipient_kind)).selected_item(), 1);
        assert_eq!(editor.view.text_input(&cx, ids!(network_url)).text(), "https://example.org/draft");
        let label = editor.reader_label(&ReaderScope::App { account: account.into(), app: "weather".into() });
        assert!(label.contains("rooms, spaces and account-wide use"));
    }

    #[test]
    fn changing_accounts_discards_stale_sharing_and_action_revoke_clicks() {
        let _account = TestAccount::set("@sharing-new:example.org");
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.account = "@sharing-old:example.org".into();
        editor.grants.push(SharingGrant {
            id: 71, source: Source::Account { account: "@sharing-old:example.org".into() },
            recipient: Recipient::NetworkOrigin("https://example.org".into()),
            reader: ReaderScope::AllReaders, duration: SharingDuration::Permanent,
        });
        editor.authorities.push(ActionAuthority {
            id: 72, context: ContextId::Agent { account: "@sharing-old:example.org".into(), room: "!old:example.org".into() },
            action: flow::SensitiveAction { kind: "network.POST".into(), target: "https://example.org".into() },
            session: AuthoritySession::RobrixSession, influences: Default::default(),
        });
        editor.effect_authorities.push(EffectAuthority {
            id: 73, context: ContextId::App { account: "@sharing-old:example.org".into(), app: "search".into(), room: None },
            recipient: Some(Recipient::NetworkOrigin("https://example.org".into())), action: None,
            operation: "operation:matrix.room.messages.search".into(), sources: Default::default(), influences: Default::default(),
            room_scope: None,
            duration: SharingDuration::Permanent,
        });


        let sharing = editor.view.button(&cx, ids!(remove_button)).widget_uid();
        editor.saved_selection = SharingCardTarget::Rule(71);
        let clicks = cx.capture_actions(|cx| {
            cx.widget_action(sharing, ButtonAction::Clicked(Default::default()));
        });
        let actions = cx.capture_actions(|cx| editor.handle_event(cx, &Event::Actions(clicks), &mut Scope::empty()));
        assert!(!actions.iter().any(|action| action.downcast_ref::<A2AppOp>().is_some()));
        assert_eq!(editor.account, "@sharing-new:example.org");
        assert!(editor.grants.is_empty());
        assert!(editor.authorities.is_empty());
        assert!(editor.effect_authorities.is_empty());
    }

    #[test]
    fn saved_operation_approval_has_readable_scope_and_can_be_removed_directly() {
        let account = "@saved-search:example.org";
        let _account = TestAccount::set(account);
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.account = account.into();
        editor.apps.push(("search".into(), "Search".into()));
        editor.rooms.push(("!private:example.org".into(), "Friends".into()));
        editor.effect_authorities.push(EffectAuthority {
            id: 74, context: ContextId::App { account: account.into(), app: "search".into(), room: Some("!private:example.org".into()) },
            recipient: Some(Recipient::NetworkOrigin("https://matrix.example.org".into())), action: None,
            operation: "operation:matrix.room.messages.search".into(),
            room_scope: None,
            sources: [Source::Room { account: account.into(), room: "!private:example.org".into() }].into(),
            influences: Default::default(), duration: SharingDuration::Permanent,
        });
        editor.open_card(&mut cx, &SharingCardTarget::EffectAuthority(74));
        let summary = editor.view.label(&cx, ids!(saved_rule_details)).text();
        assert!(summary.contains("Search · Friends"));
        assert!(summary.contains("https://matrix.example.org"));
        assert!(summary.contains("Forever"));
        assert!(summary.contains("Room · Friends"));
        assert!(!summary.contains("!private:example.org"));
        assert!(!editor.view.widget(&cx, ids!(saved_identifiers)).visible());
        for expanded in [true, true, false] {
            let actions = expand(&mut editor, &mut cx, ids!(saved_identifiers_toggle), expanded);
            assert_eq!(editor.view.widget(&cx, ids!(saved_identifiers)).visible(), expanded);
            assert!(!actions.iter().any(|action| action.downcast_ref::<A2AppOp>().is_some()));
        }
        let remove = editor.view.button(&cx, ids!(remove_button)).widget_uid();
        let clicks = cx.capture_actions(|cx| cx.widget_action(remove, ButtonAction::Clicked(Default::default())));
        let actions = cx.capture_actions(|cx| editor.handle_event(cx, &Event::Actions(clicks), &mut Scope::empty()));
        assert!(actions.iter().any(|action| matches!(action.downcast_ref::<A2AppOp>(), Some(A2AppOp::RevokeEffectAuthority(74)))));
        editor.effect_authorities.clear();
        editor.update_saved_details(&mut cx);
        assert!(!editor.view.button(&cx, ids!(remove_button)).borrow().unwrap().enabled());
    }

    fn context_fixture() -> ContextSnapshot {
        ContextSnapshot {
            epoch: 7,
            context: ContextId::Agent { account: "@alice:example.org".into(), room: "!source:example.org".into() },
            label: [Source::Room { account: "@alice:example.org".into(), room: "!source:example.org".into() }].into(),
            clearance: None,
            influences: [Influence::InternetOrigin("https://news.example".into())].into(),
        }
    }

    #[test]
    fn guided_rule_requires_review_and_keeps_the_default_grant_narrow() {
        let _account = TestAccount::set("@alice:example.org");
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.account = "@alice:example.org".into();
        let snapshot = context_fixture();
        let source = snapshot.label.first().unwrap().clone();
        editor.sources.push((source.clone(), "Private room".into()));
        editor.rooms.push(("!source:example.org".into(), "Private room".into()));
        editor.snapshots.push(snapshot.clone());
        editor.view.drop_down2(&cx, ids!(context_choice)).set_labels(&mut cx, vec!["Choose an app".into(), "This agent".into()]);
        editor.view.drop_down2(&cx, ids!(context_choice)).set_selected_item(&mut cx, 1);
        editor.view.permission_choices(&cx, ids!(reader_kind)).set_selected_item(&mut cx, 2);
        editor.view.permission_choices(&cx, ids!(sharing_lifetime)).set_selected_item(&mut cx, 2);
        editor.begin_rule(&mut cx);
        assert!(editor.wizard_open);
        assert_eq!(editor.wizard_step, 0);
        assert!(!editor.reader_options_open);
        assert_eq!(editor.view.permission_choices(&cx, ids!(reader_kind)).selected_item(), 0);
        assert_eq!(editor.view.permission_choices(&cx, ids!(sharing_lifetime)).selected_item(), 0);
        let click = |editor: &mut DataSharing, cx: &mut Cx, button: &[LiveId]| {
            let uid = editor.view.button(cx, button).widget_uid();
            let event = cx.capture_actions(|cx| cx.widget_action(uid, ButtonAction::Clicked(Default::default())));
            cx.capture_actions(|cx| editor.handle_event(cx, &Event::Actions(event), &mut Scope::empty()))
        };
        assert!(!click(&mut editor, &mut cx, ids!(add_button)).iter().any(|action| action.downcast_ref::<A2AppOp>().is_some()));
        click(&mut editor, &mut cx, ids!(wizard_next));
        assert_eq!(editor.wizard_step, 1);
        editor.view.permission_choices(&cx, ids!(recipient_kind)).set_selected_item(&mut cx, 1);
        click(&mut editor, &mut cx, ids!(wizard_next));
        assert_eq!(editor.wizard_step, 1, "an empty website cannot advance to approval");
        editor.view.text_input(&cx, ids!(network_url)).set_text(&mut cx, "https://example.org/news");
        let next = editor.view.button(&cx, ids!(wizard_next)).widget_uid();
        let allow = editor.view.button(&cx, ids!(add_button)).widget_uid();
        let stale = cx.capture_actions(|cx| {
            cx.widget_action(next, ButtonAction::Clicked(Default::default()));
            cx.widget_action(allow, ButtonAction::Clicked(Default::default()));
        });
        let actions = cx.capture_actions(|cx| editor.handle_event(cx, &Event::Actions(stale), &mut Scope::empty()));
        assert!(!actions.iter().any(|action| action.downcast_ref::<A2AppOp>().is_some()),
            "advancing to review must not also accept a stale Allow click");
        assert_eq!(editor.wizard_step, 2);
        let preview = editor.view.label(&cx, ids!(rule_preview)).text();
        assert!(preview.contains("@alice:example.org"));
        assert!(preview.contains("Private room"));
        assert!(!preview.contains("!source:example.org"));
        assert_eq!(preview.matches("@alice:example.org").count(), 1);
        assert!(preview.contains("https://example.org"));
        assert!(!editor.view.widget(&cx, ids!(rule_identifiers)).visible());
        let actions = expand(&mut editor, &mut cx, ids!(rule_identifiers_toggle), true);
        assert!(editor.view.widget(&cx, ids!(rule_identifiers)).visible());
        assert!(editor.view.label(&cx, ids!(rule_identifiers)).text().contains("!source:example.org"));
        assert!(!actions.iter().any(|action| action.downcast_ref::<A2AppOp>().is_some()));
        let actions = click(&mut editor, &mut cx, ids!(add_button));
        assert!(actions.iter().any(|action| matches!(action.downcast_ref::<A2AppOp>(), Some(A2AppOp::GrantFlowSharing {
            source: actual, reader: ReaderScope::Context(context), recipient: Recipient::NetworkOrigin(origin), duration: SharingDuration::RobrixSession,
        }) if actual == &source && context == &snapshot.context && origin == "https://example.org")));
        let actions = click(&mut editor, &mut cx, ids!(wizard_cancel));
        assert!(!editor.wizard_open);
        assert!(!actions.iter().any(|action| action.downcast_ref::<A2AppOp>().is_some()));
        assert!(!click(&mut editor, &mut cx, ids!(add_button)).iter().any(|action| action.downcast_ref::<A2AppOp>().is_some()));
        click(&mut editor, &mut cx, ids!(wizard_next));
        assert!(!editor.wizard_open, "stale navigation must not reopen a cancelled rule");
    }

    #[test]
    fn sharing_editor_keeps_reader_and_lifetime_narrow_until_explicitly_changed() {
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.account = "@alice:example.org".into();
        let snapshot = context_fixture();
        let source = snapshot.label.first().unwrap().clone();
        editor.sources.push((source.clone(), "Source".into()));
        editor.snapshots.push(snapshot.clone());
        editor.view.drop_down2(&cx, ids!(context_choice)).set_labels(&mut cx, vec!["Select context".into(), "Fixture context".into()]);
        editor.view.drop_down2(&cx, ids!(context_choice)).set_selected_item(&mut cx, 1);
        editor.apps.push(("weather".into(), "Weather".into()));
        editor.rooms.push(("!expiry:example.org".into(), "Expiry".into()));
        editor.rooms.push(("!source:example.org".into(), "Private room".into()));
        editor.view.permission_choices(&cx, ids!(recipient_kind)).set_selected_item(&mut cx, 1);
        editor.view.text_input(&cx, ids!(network_url)).set_text(&mut cx, "https://example.org/private/path");
        editor.update_recipient_form(&mut cx);
        assert!(editor.view.button(&cx, ids!(add_button)).borrow().unwrap().enabled());
        let preview = editor.view.label(&cx, ids!(rule_preview)).text();
        assert!(preview.contains("Private room"));
        assert!(!preview.contains("!source:example.org"));
        assert!(editor.view.label(&cx, ids!(rule_identifiers)).text().contains("!source:example.org"));
        assert!(preview.contains("@alice:example.org"));
        assert!(preview.contains("https://example.org"));
        assert!(preview.contains("Until Robrix closes"));
        editor.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 2);
        editor.update_page(&mut cx);
        editor.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 0);
        editor.update_page(&mut cx);
        match editor.sharing_action(&cx).unwrap() {
            A2AppOp::GrantFlowSharing { source: actual, reader, duration, recipient } => {
                assert_eq!(actual, source);
                assert_eq!(reader, ReaderScope::Context(snapshot.context));
                assert_eq!(duration, SharingDuration::RobrixSession);
                assert_eq!(recipient, Recipient::NetworkOrigin("https://example.org".into()));
            }
            _ => panic!("wrong action"),
        }
        editor.view.permission_choices(&cx, ids!(reader_kind)).set_selected_item(&mut cx, 1);
        editor.view.permission_choices(&cx, ids!(sharing_lifetime)).set_selected_item(&mut cx, 1);
        match editor.sharing_action(&cx).unwrap() {
            A2AppOp::GrantFlowSharing { reader, duration, .. } => {
                assert_eq!(reader, ReaderScope::App { account: editor.account.clone(), app: "weather".into() });
                assert_eq!(duration, SharingDuration::RoomSession { account: editor.account.clone(), room: "!expiry:example.org".into() });
            }
            _ => panic!("wrong action"),
        }
        editor.sources[0].0 = Source::UnknownPrivate;
        editor.update_recipient_form(&mut cx);
        assert!(!editor.view.button(&cx, ids!(add_button)).borrow().unwrap().enabled());
        assert!(editor.sharing_action(&cx).is_err());
    }

    #[test]
    fn action_review_binds_exact_context_target_and_displayed_influences() {
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        let snapshot = context_fixture();
        let action = flow::SensitiveAction { kind: "network.POST".into(), target: "https://example.org".into() };
        editor.snapshots.push(snapshot.clone());
        editor.view.drop_down2(&cx, ids!(context_choice)).set_labels(&mut cx, vec!["Select context".into(), "Fixture context".into()]);
        editor.view.drop_down2(&cx, ids!(context_choice)).set_selected_item(&mut cx, 1);
        editor.action_decisions.push(ActionDecision {
            context: snapshot.context.clone(), epoch: snapshot.epoch, action: action.clone(), influences: snapshot.influences.clone(), allowed: false,
            request: None,
        });

        editor.action_selection = 1;
        editor.view.permission_choices(&cx, ids!(action_session)).set_selected_item(&mut cx, 1);
        editor.update_diagnostic_details(&mut cx);
        match editor.authority_action(&cx).unwrap() {
            A2AppOp::GrantFlowAuthority { context, action: actual, session, expected_influences, expected_epoch } => {
                assert_eq!(context, snapshot.context);
                assert_eq!(actual, action);
                assert_eq!(expected_influences, snapshot.influences);
                assert_eq!(expected_epoch, snapshot.epoch);
                assert_eq!(session, AuthoritySession::RoomSession { account: "@alice:example.org".into(), room: "!source:example.org".into() });
            }
            _ => panic!("wrong action"),
        }
        editor.snapshots[0].epoch += 1;
        assert!(editor.authority_action(&cx).is_err(), "a reopened context must not revive an old action review");
        editor.snapshots.clear();
        assert!(editor.authority_action(&cx).is_err());
    }

    #[test]
    fn blocked_decision_prefills_only_its_actual_source_recipient_and_context() {
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.account = "@alice:example.org".into();
        let snapshot = context_fixture();
        editor.snapshots.push(snapshot.clone());
        editor.view.drop_down2(&cx, ids!(context_choice)).set_labels(&mut cx, vec!["Select context".into(), "Fixture context".into()]);
        editor.view.drop_down2(&cx, ids!(context_choice)).set_selected_item(&mut cx, 1);
        let source = snapshot.label.first().unwrap().clone();
        editor.decisions.push(FlowDecision {
            context: snapshot.context.clone(), epoch: snapshot.epoch, recipient: Recipient::NetworkOrigin("https://blocked.example:8443".into()),
            sources: snapshot.label.clone(), denied_sources: snapshot.label, allowed: false,
        });

        select_first(&mut editor);
        editor.update_diagnostic_details(&mut cx);
        editor.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 1);
        editor.update_page(&mut cx);
        editor.review_decision(&mut cx).unwrap();
        assert_eq!(editor.view.permission_choices(&cx, ids!(page_choice)).selected_item(), 0);
        assert_eq!(editor.selected_source(&cx).unwrap(), source);
        assert_eq!(editor.selected_reader(&cx).unwrap(), ReaderScope::Context(snapshot.context));
        assert_eq!(editor.selected_recipient(&cx).unwrap(), Recipient::NetworkOrigin("https://blocked.example:8443".into()));
        editor.decisions[0].recipient = Recipient::ModelProvider("stale-model-identity".into());
        select_first(&mut editor);
        assert!(editor.review_decision(&mut cx).is_err());
        editor.decisions[0].recipient = Recipient::Clipboard;
        select_first(&mut editor);
        editor.review_decision(&mut cx).unwrap();
        assert_eq!(editor.selected_recipient(&cx).unwrap(), Recipient::Clipboard);
        assert!(editor.view.widget(&cx, ids!(clipboard_section)).visible());
    }

    #[test]
    fn stopped_or_restarted_sharing_request_keeps_its_original_scope() {
        for epoch in [0, 8] {
            let (mut cx, widget) = editor();
            let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
            editor.account = "@alice:example.org".into();
            let mut snapshot = context_fixture();
            let source = snapshot.label.first().unwrap().clone();
            let context = snapshot.context.clone();
            editor.decisions.push(FlowDecision {
                context: context.clone(), epoch: snapshot.epoch,
                recipient: Recipient::NetworkOrigin("https://blocked.example".into()),
                sources: snapshot.label.clone(), denied_sources: snapshot.label.clone(), allowed: false,
            });
            snapshot.epoch = epoch;
            editor.snapshots.push(snapshot);
            editor.view.drop_down2(&cx, ids!(context_choice)).set_labels(&mut cx, vec!["Select context".into(), "Fixture context".into()]);
            select_first(&mut editor);
            editor.update_diagnostic_details(&mut cx);
            assert!(!editor.view.widget(&cx, ids!(review_button)).disabled(&cx));
            editor.review_decision(&mut cx).unwrap();
            match editor.sharing_action(&cx).unwrap() {
                A2AppOp::GrantFlowSharing { source: actual, recipient, reader, duration } => {
                    assert_eq!(actual, source);
                    assert_eq!(recipient, Recipient::NetworkOrigin("https://blocked.example".into()));
                    assert_eq!(reader, ReaderScope::Context(context));
                    assert_eq!(duration, SharingDuration::RobrixSession);
                }
                _ => panic!("expected a sharing rule"),
            }
        }
    }

    #[test]
    fn blocked_sharing_explains_unreviewable_sources_and_hides_identifiers() {
        let _account = TestAccount::set("@alice:example.org");
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.account = "@alice:example.org".into();
        editor.rooms.push(("!source:example.org".into(), "Private room".into()));
        let snapshot = context_fixture();
        editor.snapshots.push(snapshot.clone());
        editor.decisions.push(FlowDecision {
            context: snapshot.context, epoch: snapshot.epoch,
            recipient: Recipient::NetworkOrigin("https://blocked.example".into()),
            sources: [Source::UnknownPrivate].into(), denied_sources: [Source::UnknownPrivate].into(), allowed: false,
        });
        select_first(&mut editor);
        editor.update_diagnostic_details(&mut cx);
        assert!(editor.view.widget(&cx, ids!(review_button)).disabled(&cx));
        assert!(editor.view.label(&cx, ids!(review_problem)).text().contains("cannot identify"));
        assert!(!editor.view.widget(&cx, ids!(allow_all_section)).visible(), "no rule can release it, so there is nothing to allow");
        assert!(editor.allow_all_grants(&cx).is_err());
        assert!(editor.review_decision(&mut cx).is_err());
        assert!(!editor.wizard_open);
        let summary = editor.view.label(&cx, ids!(decision_details)).text();
        assert!(summary.contains("Private room"));
        assert!(!summary.contains("@alice:example.org"));
        assert!(!summary.contains("!source:example.org"));
        assert!(!editor.view.widget(&cx, ids!(decision_identifiers)).visible());
        let details = editor.view.label(&cx, ids!(decision_identifiers)).text();
        assert!(details.contains("@alice:example.org"));
        assert!(details.contains("!source:example.org"));
        editor.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 1);
        for expanded in [true, true, false] {
            let actions = expand(&mut editor, &mut cx, ids!(decision_identifiers_toggle), expanded);
            assert_eq!(editor.view.widget(&cx, ids!(decision_identifiers)).visible(), expanded);
            assert!(!actions.iter().any(|action| action.downcast_ref::<A2AppOp>().is_some()));
        }
    }

    #[test]
    fn exact_action_review_uses_the_displayed_request_and_rejects_a_closed_activation() {
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        let snapshot = context_fixture();
        editor.snapshots.push(snapshot.clone());
        editor.action_decisions.push(ActionDecision {
            context: snapshot.context.clone(), epoch: snapshot.epoch,
            action: flow::SensitiveAction { kind: "matrix.message.send".into(), target: "!target:example.org".into() },
            influences: snapshot.influences.clone(), allowed: false,
            request: Some(flow::ActionRequest { id: 42, payload: "{\"body\":\"The exact reviewed message\"}".into() }),
        });

        editor.action_selection = 1;
        editor.update_diagnostic_details(&mut cx);
        assert!(editor.view.label(&cx, ids!(action_details)).text().contains("The exact reviewed message"));
        assert!(!editor.view.widget(&cx, ids!(authority_button)).disabled(&cx));
        match editor.authority_action(&cx).unwrap() {
            A2AppOp::GrantExactFlowAuthority { request_id, context, expected_epoch, expected_influences } => {
                assert_eq!(request_id, 42);
                assert_eq!(context, snapshot.context);
                assert_eq!(expected_epoch, snapshot.epoch);
                assert_eq!(expected_influences, snapshot.influences);
            }
            _ => panic!("Once must use the exact captured request"),
        }
        editor.action_decisions[0].request = None;
        editor.update_diagnostic_details(&mut cx);
        assert!(editor.view.widget(&cx, ids!(authority_button)).disabled(&cx));
        assert!(!editor.view.button(&cx, ids!(authority_button)).borrow().unwrap().enabled());
        assert!(editor.authority_action(&cx).is_err());
        editor.view.permission_choices(&cx, ids!(action_session)).set_selected_item(&mut cx, 2);
        editor.update_diagnostic_details(&mut cx);
        assert!(!editor.view.widget(&cx, ids!(authority_button)).disabled(&cx),
            "an explicit session choice may review an operation without exact contents");
        editor.snapshots[0].epoch += 1;
        editor.update_diagnostic_details(&mut cx);
        assert!(editor.view.widget(&cx, ids!(authority_button)).disabled(&cx));
        assert!(editor.authority_action(&cx).is_err());
        editor.snapshots[0].epoch = 0;
        editor.action_decisions[0].epoch = 0;
        editor.update_diagnostic_details(&mut cx);
        assert!(editor.view.widget(&cx, ids!(authority_button)).disabled(&cx));
        assert!(editor.authority_action(&cx).is_err(), "retained compartments cannot approve actions while stopped");
    }

    #[test]
    fn selecting_another_action_resets_session_approval_to_once() {
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        let snapshot = context_fixture();
        editor.snapshots.push(snapshot.clone());
        editor.action_decisions = [42, 43].into_iter().map(|id| ActionDecision {
            context: snapshot.context.clone(), epoch: snapshot.epoch,
            action: flow::SensitiveAction { kind: "matrix.message.send".into(), target: "!target:example.org".into() },
            influences: snapshot.influences.clone(), allowed: false,
            request: Some(flow::ActionRequest { id, payload: format!("{{\"body\":\"Message {id}\"}}").into() }),
        }).collect();

        editor.action_selection = 1;
        editor.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 1);
        editor.view.permission_choices(&cx, ids!(action_session)).set_selected_item(&mut cx, 2);
        assert!(matches!(editor.authority_action(&cx).unwrap(), A2AppOp::GrantFlowAuthority { session: AuthoritySession::RobrixSession, .. }));
        editor.action_selection = 0;
        let changed = cx.capture_actions(|cx| cx.action(SharingCardAction {
            owner: editor.widget_uid(), account: editor.account.clone(),
            target: SharingCardTarget::Action(editor.action_decisions[1].clone()),
        }));
        editor.handle_event(&mut cx, &Event::Actions(changed), &mut Scope::empty());
        assert_eq!(editor.view.permission_choices(&cx, ids!(action_session)).selected_item(), 0);
        assert!(matches!(editor.authority_action(&cx).unwrap(), A2AppOp::GrantExactFlowAuthority { request_id: 43, .. }));
        assert_eq!(editor.view.button(&cx, ids!(authority_button)).text(), "Allow this exact action once");
    }

    #[test]
    fn leaving_action_review_discards_stale_approval_and_hidden_card_actions() {
        let _account = TestAccount::set("@alice:example.org");
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.account = "@alice:example.org".into();
        let snapshot = context_fixture();
        editor.snapshots.push(snapshot.clone());
        let decision = ActionDecision {
            context: snapshot.context, epoch: snapshot.epoch, influences: snapshot.influences,
            action: flow::SensitiveAction { kind: "matrix.message.send".into(), target: "!target:example.org".into() },
            allowed: false, request: Some(flow::ActionRequest { id: 42, payload: "{\"body\":\"Reviewed message\"}".into() }),
        };
        editor.action_decisions.push(decision.clone());
        editor.action_selection = 1;
        editor.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 1);
        assert!(editor.authority_action(&cx).is_ok());
        let back = editor.view.button(&cx, ids!(attention_back)).widget_uid();
        let allow = editor.view.button(&cx, ids!(authority_button)).widget_uid();
        let stale = cx.capture_actions(|cx| {
            cx.widget_action(back, ButtonAction::Clicked(Default::default()));
            cx.widget_action(allow, ButtonAction::Clicked(Default::default()));
        });
        let actions = cx.capture_actions(|cx| editor.handle_event(cx, &Event::Actions(stale), &mut Scope::empty()));
        assert_eq!(editor.action_selection, 0);
        assert!(!actions.iter().any(|action| action.downcast_ref::<A2AppOp>().is_some()));

        editor.action_selection = 1;
        editor.begin_rule(&mut cx);
        let stale = cx.capture_actions(|cx| {
            cx.widget_action(allow, ButtonAction::Clicked(Default::default()));
            cx.action(SharingCardAction { owner: editor.widget_uid(), account: editor.account.clone(), target: SharingCardTarget::Action(decision) });
        });
        let actions = cx.capture_actions(|cx| editor.handle_event(cx, &Event::Actions(stale), &mut Scope::empty()));
        assert!(editor.wizard_open);
        assert!(!actions.iter().any(|action| action.downcast_ref::<A2AppOp>().is_some()), "a hidden action review cannot grant authority while adding a sharing rule");
    }

    #[test]
    fn back_returns_through_wizard_and_request_before_leaving_sharing() {
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.wizard_open = true;
        editor.wizard_step = 2;
        editor.wizard_return_page = 1;
        let request = (context_fixture().context, Recipient::Clipboard);
        editor.decision_selection = Some(request.clone());
        for step in [1, 0] {
            assert!(editor.back(&mut cx));
            assert_eq!(editor.wizard_step, step);
            assert!(editor.wizard_open);
        }
        assert!(editor.back(&mut cx));
        assert!(!editor.wizard_open);
        assert_eq!(editor.view.permission_choices(&cx, ids!(page_choice)).selected_item(), 1);
        assert_eq!(editor.decision_selection, Some(request), "leaving a rule returns to the request that opened it");
        assert!(editor.back(&mut cx));
        assert!(editor.decision_selection.is_none());
        assert!(!editor.back(&mut cx));
        editor.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 0);
        editor.saved_selection = SharingCardTarget::Rule(1);
        assert!(editor.back(&mut cx));
        assert!(matches!(editor.saved_selection, SharingCardTarget::None));
        editor.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 2);
        editor.activity_selection = Some(1);
        assert!(editor.back(&mut cx));
        assert!(editor.activity_selection.is_none());
        assert!(!editor.back(&mut cx));
    }

    #[test]
    fn sharing_denials_name_the_blocking_control_and_how_to_change_it() {
        let (_cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.account = "@alice:example.org".into();
        let snapshot = context_fixture();
        let recipient = Recipient::NetworkOrigin("https://example.org".into());
        let text = editor.sharing_remedy(&snapshot.label, &recipient);
        assert!(text.contains("Sharing needs your approval"));
        assert!(text.contains("Review sharing rule"));
        assert!(text.contains("Allow sharing"));
        assert!(editor.sharing_remedy(&[Source::UnknownPrivate].into(), &recipient).contains("No sharing rule"));
        let several = [Source::Account { account: editor.account.clone() }, Source::RoomDirectory { account: editor.account.clone() }].into();
        assert!(editor.sharing_remedy(&several, &recipient).contains("Allow all listed data"));
    }

    #[test]
    fn source_options_list_the_directory_after_account_data_and_keep_a_reviewed_source() {
        assert!(source_options("", &[], [], None).is_empty());
        let account = "@alice:example.org";
        let room = |id: &str| Source::Room { account: account.into(), room: id.into() };
        let directory = Source::RoomDirectory { account: account.into() };
        let foreign = Source::Room { account: "@bob:example.org".into(), room: "!bob:example.org".into() };
        let rooms = vec![("!joined:example.org".to_string(), "Joined".to_string())];
        let options = source_options(account, &rooms, [room("!left:example.org"), foreign.clone(), directory.clone()], Some(&room("!reviewed:example.org")));
        assert_eq!(options.iter().map(|(source, _)| source.clone()).collect::<Vec<_>>(), vec![
            Source::Account { account: account.into() }, directory, room("!joined:example.org"),
            room("!left:example.org"), room("!reviewed:example.org"),
        ]);
        assert_eq!(options[1].1, "Room and space directory");
        for kept in [foreign, Source::UnknownPrivate] {
            assert!(!source_options(account, &rooms, [], Some(&kept)).iter().any(|(source, _)| source == &kept), "only the account's own data can be kept");
        }
    }

    #[test]
    fn directory_source_builds_the_agent_rule_for_its_own_room() {
        let account = unique_account("directory-rule");
        let _account = TestAccount::set(&account);
        let room = "!directory-rule-ai:example.org";
        let context = crate::a2app::information_flow::prepare_agent(room).unwrap();
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.configure(&mut cx);
        let directory = Source::RoomDirectory { account: account.clone() };
        assert_eq!(editor.sources.iter().position(|(source, _)| source == &directory), Some(1), "listed right after account data");
        editor.view.drop_down2(&cx, ids!(source_choice)).set_selected_item(&mut cx, 1);
        let index = editor.snapshots.iter().position(|snapshot| snapshot.context == context).unwrap();
        editor.view.drop_down2(&cx, ids!(context_choice)).set_selected_item(&mut cx, index + 1);
        editor.rooms.push((room.into(), "AI room".into()));
        editor.view.permission_choices(&cx, ids!(recipient_kind)).set_selected_item(&mut cx, 2);
        editor.view.permission_choices(&cx, ids!(sharing_lifetime)).set_selected_item(&mut cx, 2);
        assert_eq!(editor.sharing_draft(&cx), Some((directory, Recipient::MatrixRoom { account: account.clone(), room: room.into() },
            ReaderScope::Context(context), SharingDuration::Permanent)));
        editor.refresh_rules(&mut cx);
        assert!(editor.view.label(&cx, ids!(source_default)).text().contains("AI rooms share it"));
    }

    #[test]
    fn homeserver_destination_is_the_origin_ai_cards_are_checked_against() {
        let account = unique_account("homeserver-destination");
        let _account = TestAccount::set(&account);
        let _model = TestModel::set(Err("No AI service in this test.".into()));
        let _homeserver = TestHomeserver::set("https://matrix-client.example.org/");
        let room = "!homeserver-destination-ai:example.org";
        let context = crate::a2app::information_flow::prepare_agent(room).unwrap();
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.configure(&mut cx);
        // The same expression the AI room worker checks its cards against.
        let homeserver = Recipient::network_origin("https://matrix-client.example.org/").unwrap();
        editor.view.permission_choices(&cx, ids!(recipient_kind)).set_selected_item(&mut cx, 4);
        editor.update_recipient_form(&mut cx);
        assert_eq!(editor.selected_recipient(&cx).unwrap(), homeserver);
        assert!(editor.view.widget(&cx, ids!(homeserver_section)).visible());
        assert!(editor.view.label(&cx, ids!(homeserver_description)).text().contains("https://matrix-client.example.org"));
        assert_eq!(editor.recipient_choice_label(&homeserver), "My Matrix homeserver");
        assert!(editor.recipient_label(&homeserver).contains("https://matrix-client.example.org"));
        // A blocked check against that origin is reviewed as the homeserver, not as a typed website.
        assert!(!flow::decision(&context, &homeserver).unwrap().allowed);
        editor.view.permission_choices(&cx, ids!(recipient_kind)).set_selected_item(&mut cx, 0);
        editor.refresh_diagnostics(&mut cx);
        select_first(&mut editor);
        editor.update_diagnostic_details(&mut cx);
        editor.review_decision(&mut cx).unwrap();
        assert_eq!(editor.view.permission_choices(&cx, ids!(recipient_kind)).selected_item(), 4);
        assert_eq!(editor.selected_recipient(&cx).unwrap(), homeserver);
        assert!(editor.view.text_input(&cx, ids!(network_url)).text().is_empty());
    }

    #[test]
    fn needs_attention_drops_approved_data_and_says_once_to_try_again() {
        let account = unique_account("attention-resolved");
        let _account = TestAccount::set(&account);
        let _model = TestModel::set(Err("No AI service in this test.".into()));
        let room = "!attention-resolved-ai:example.org";
        let context = crate::a2app::information_flow::prepare_agent(room).unwrap();
        let own = Source::Room { account: account.clone(), room: room.into() };
        let directory = Source::RoomDirectory { account: account.clone() };
        flow::add_sources(&context, [directory.clone()]).unwrap();
        let website = Recipient::NetworkOrigin("https://attention-resolved.example".into());
        assert!(!flow::decision(&context, &website).unwrap().allowed);
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.configure(&mut cx);
        editor.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 1);
        assert_eq!(editor.decisions.len(), 1);
        let card = SharingCardTarget::Decision(editor.decisions[0].clone());
        editor.open_card(&mut cx, &card);
        assert_eq!(editor.blocked_sources, vec![own.clone(), directory.clone()]);
        editor.view.permission_choices(&cx, ids!(blocked_source_choice)).set_selected_item(&mut cx, 1);
        let reader = ReaderScope::Context(context.clone());
        flow::grant_sharing(own, website.clone(), reader.clone(), SharingDuration::RobrixSession).unwrap();
        editor.refresh_diagnostics(&mut cx);
        assert_eq!(editor.decision_selection, Some((context.clone(), website.clone())), "the open request survives the change");
        assert_eq!(editor.blocked_sources, vec![directory.clone()], "an allowed source is no longer listed");
        assert_eq!(editor.view.permission_choices(&cx, ids!(blocked_source_choice)).selected_item(), 0, "the chosen source stays chosen");
        assert!(!editor.view.label(&cx, ids!(decision_details)).text().contains("Allowed now"));
        flow::grant_sharing(directory, website.clone(), reader, SharingDuration::RobrixSession).unwrap();
        editor.refresh_diagnostics(&mut cx);
        assert!(editor.view.label(&cx, ids!(decision_details)).text().contains("Allowed now. Send your message again."));
        assert!(!editor.view.widget(&cx, ids!(review_section)).visible());
        assert!(!editor.view.widget(&cx, ids!(allow_all_section)).visible());
        assert!(!editor.card_is_visible(&card), "a resolved request has no card");
        assert!(editor.back(&mut cx));
        editor.refresh_diagnostics(&mut cx);
        assert!(editor.decisions.is_empty(), "it is shown only once");
        assert!(editor.view.label(&cx, ids!(attention_summary)).text().starts_with("Nothing needs your attention"));
    }

    #[test]
    fn needs_attention_drops_data_a_new_session_no_longer_holds() {
        let account = unique_account("attention-relabelled");
        let _account = TestAccount::set(&account);
        let room = "!attention-relabelled-ai:example.org";
        let context = crate::a2app::information_flow::prepare_agent(room).unwrap();
        let other = Source::Room { account: account.clone(), room: "!attention-relabelled-other:example.org".into() };
        flow::add_sources(&context, [other]).unwrap();
        let website = Recipient::NetworkOrigin("https://attention-relabelled.example".into());
        flow::decision(&context, &website).unwrap();
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.configure(&mut cx);
        assert_eq!(editor.decisions.len(), 1);
        assert_eq!(editor.decisions[0].denied_sources.len(), 2);
        crate::a2app::information_flow::begin_agent_session(room).unwrap();
        editor.refresh_diagnostics(&mut cx);
        assert_eq!(editor.decisions[0].denied_sources, [Source::Room { account: account.clone(), room: room.into() }].into(),
            "only the data the agent still holds needs a rule");
    }

    #[test]
    fn an_open_request_whose_data_left_the_label_is_not_called_allowed() {
        let account = unique_account("attention-unheld");
        let _account = TestAccount::set(&account);
        let _model = TestModel::set(Err("No AI service in this test.".into()));
        let room = "!attention-unheld-ai:example.org";
        let context = crate::a2app::information_flow::prepare_agent(room).unwrap();
        let own = Source::Room { account: account.clone(), room: room.into() };
        let other = Source::Room { account: account.clone(), room: "!attention-unheld-other:example.org".into() };
        let website = Recipient::NetworkOrigin("https://attention-unheld.example".into());
        flow::grant_sharing(own, website.clone(), ReaderScope::Context(context.clone()), SharingDuration::RobrixSession).unwrap();
        flow::add_sources(&context, [other.clone()]).unwrap();
        assert_eq!(flow::decision(&context, &website).unwrap().denied_sources, [other].into());
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.configure(&mut cx);
        editor.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 1);
        let card = SharingCardTarget::Decision(editor.decisions[0].clone());
        editor.open_card(&mut cx, &card);
        // A new session drops the other room from the label, but no rule allows it.
        crate::a2app::information_flow::begin_agent_session(room).unwrap();
        editor.refresh_diagnostics(&mut cx);
        let details = editor.view.label(&cx, ids!(decision_details)).text();
        assert!(details.contains("The agent no longer holds this data") && !details.contains("Allowed now"), "{details}");
        assert!(!editor.view.widget(&cx, ids!(review_section)).visible());
        assert!(!editor.card_is_visible(&card), "it no longer blocks anything");
        assert!(editor.back(&mut cx));
        editor.refresh_diagnostics(&mut cx);
        assert!(editor.decisions.is_empty(), "it is shown only once");
    }

    #[test]
    fn allow_all_listed_data_grants_each_still_blocked_source_once() {
        let account = unique_account("allow-all");
        let _account = TestAccount::set(&account);
        let _model = TestModel::set(Err("No AI service in this test.".into()));
        let room = "!allow-all-ai:example.org";
        let context = crate::a2app::information_flow::prepare_agent(room).unwrap();
        let own = Source::Room { account: account.clone(), room: room.into() };
        let other = Source::Room { account: account.clone(), room: "!allow-all-other:example.org".into() };
        let directory = Source::RoomDirectory { account: account.clone() };
        flow::add_sources(&context, [other.clone(), directory.clone()]).unwrap();
        let website = Recipient::NetworkOrigin("https://allow-all.example".into());
        flow::decision(&context, &website).unwrap();
        let reader = ReaderScope::Context(context.clone());
        flow::grant_sharing(own, website.clone(), reader.clone(), SharingDuration::RobrixSession).unwrap();
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.configure(&mut cx);
        editor.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 1);
        let card = SharingCardTarget::Decision(editor.decisions[0].clone());
        editor.open_card(&mut cx, &card);
        assert!(editor.view.widget(&cx, ids!(allow_all_section)).visible());
        assert!(!editor.view.widget(&cx, ids!(allow_all_button)).disabled(&cx));
        assert_eq!(editor.view.permission_choices(&cx, ids!(allow_all_lifetime)).selected_item(), 0, "the narrow duration is the default");
        editor.view.permission_choices(&cx, ids!(allow_all_lifetime)).set_selected_item(&mut cx, 2);
        let actions = press(&mut editor, &mut cx, ids!(allow_all_button));
        assert_eq!(granted(&actions), vec![
            (other.clone(), website.clone(), reader.clone(), SharingDuration::Permanent),
            (directory.clone(), website.clone(), reader.clone(), SharingDuration::Permanent),
        ], "one rule per blocked source, none for the already allowed room");
        // A source allowed after the list was drawn gets no second rule.
        flow::grant_sharing(other, website.clone(), reader.clone(), SharingDuration::RobrixSession).unwrap();
        editor.view.permission_choices(&cx, ids!(allow_all_lifetime)).set_selected_item(&mut cx, 1);
        let actions = press(&mut editor, &mut cx, ids!(allow_all_button));
        assert_eq!(granted(&actions), vec![(directory, website, reader,
            SharingDuration::RoomSession { account: account.clone(), room: room.into() })]);
    }

    #[test]
    fn reopening_the_pane_keeps_an_open_review_and_never_switches_its_source() {
        let account = unique_account("review-survives");
        let _account = TestAccount::set(&account);
        let _model = TestModel::set(Ok(test_model("model-one")));
        let _homeserver = TestHomeserver::set("https://hs.review-survives.example");
        let room = "!review-survives-ai:example.org";
        let context = crate::a2app::information_flow::prepare_agent(room).unwrap();
        let left = Source::Room { account: account.clone(), room: "!review-survives-left:example.org".into() };
        let directory = Source::RoomDirectory { account: account.clone() };
        flow::add_sources(&context, [left.clone(), directory.clone()]).unwrap();
        let homeserver = Recipient::network_origin("https://hs.review-survives.example").unwrap();
        flow::decision(&context, &homeserver).unwrap();
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.configure(&mut cx);
        editor.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 1);
        let card = SharingCardTarget::Decision(editor.decisions[0].clone());
        editor.open_card(&mut cx, &card);
        // The directory is always listed; a room left since then is kept for the open review.
        for source in [directory, left] {
            let index = editor.blocked_sources.iter().position(|candidate| candidate == &source).unwrap();
            editor.view.permission_choices(&cx, ids!(blocked_source_choice)).set_selected_item(&mut cx, index);
            editor.review_decision(&mut cx).unwrap();
            editor.configure(&mut cx);
            assert!(editor.wizard_open, "reopening the pane keeps the review open");
            assert_eq!(editor.selected_source(&cx).unwrap(), source, "and keeps its data source");
            assert_eq!(editor.selected_recipient(&cx).unwrap(), homeserver);
            assert_eq!(editor.decision_selection, Some((context.clone(), homeserver.clone())));
            press(&mut editor, &mut cx, ids!(wizard_cancel));
            assert_eq!(editor.view.permission_choices(&cx, ids!(page_choice)).selected_item(), 1);
        }
        // A rule whose destination changed underneath it is closed, not retargeted.
        editor.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 0);
        editor.decision_selection = None;
        editor.begin_rule(&mut cx);
        let index = editor.snapshots.iter().position(|snapshot| snapshot.context == context).unwrap();
        editor.view.drop_down2(&cx, ids!(context_choice)).set_selected_item(&mut cx, index + 1);
        editor.view.permission_choices(&cx, ids!(recipient_kind)).set_selected_item(&mut cx, 0);
        editor.wizard_step = 2;
        assert!(matches!(editor.sharing_draft(&cx), Some((_, Recipient::ModelProvider(id), ..)) if id == "model-one"));
        editor.configure(&mut cx);
        assert!(editor.wizard_open, "an unchanged service keeps the rule open");
        TEST_MODEL.with(|current| current.replace(Some(Ok(test_model("model-two")))));
        editor.configure(&mut cx);
        assert_eq!(editor.model.as_ref().map(|model| model.id.as_str()), Some("model-two"), "every configure re-reads the AI service");
        assert!(!editor.wizard_open, "a changed AI service closes the rule instead of switching it");
    }

    #[test]
    fn model_requests_wait_for_a_checking_service_instead_of_calling_it_changed() {
        let _model = TestModel::set(Err("Checking Claude Code sign-in. Try again shortly.".into()));
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.account = "@alice:example.org".into();
        let snapshot = context_fixture();
        editor.snapshots.push(snapshot.clone());
        editor.view.drop_down2(&cx, ids!(context_choice)).set_labels(&mut cx, vec!["Select context".into(), "Fixture context".into()]);
        editor.decisions.push(FlowDecision {
            context: snapshot.context.clone(), epoch: snapshot.epoch, recipient: Recipient::ModelProvider("model-one".into()),
            sources: snapshot.label.clone(), denied_sources: snapshot.label.clone(), allowed: false,
        });
        select_first(&mut editor);
        editor.refresh_model(&mut cx);
        editor.update_diagnostic_details(&mut cx);
        let problem = editor.view.label(&cx, ids!(review_problem)).text();
        assert!(problem.contains("Checking Claude Code sign-in") && !problem.contains("changed"), "{problem}");
        assert!(!editor.view.widget(&cx, ids!(review_button)).disabled(&cx), "pressing it again reads the service again");
        assert!(!editor.view.widget(&cx, ids!(allow_all_button)).disabled(&cx));
        assert!(editor.review_decision(&mut cx).is_err());
        TEST_MODEL.with(|current| current.replace(Some(Ok(test_model("model-one")))));
        editor.review_decision(&mut cx).unwrap();
        assert_eq!(editor.selected_recipient(&cx).unwrap(), Recipient::ModelProvider("model-one".into()), "the review re-reads the service");
    }

    #[cfg(unix)]
    #[test]
    fn ai_room_requests_and_rules_offer_to_restore_the_room_defaults() {
        let account = "@alice:example.org";
        let _account = TestAccount::set(account);
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.account = account.into();
        let snapshot = context_fixture();
        let room = "!source:example.org";
        let directory = Source::RoomDirectory { account: account.into() };
        let own_room = Recipient::MatrixRoom { account: account.into(), room: room.into() };
        editor.snapshots.push(snapshot.clone());
        editor.rooms.push((room.into(), "AI room".into()));
        for recipient in [own_room.clone(), Recipient::NetworkOrigin("https://example.org".into())] {
            editor.decisions.push(FlowDecision { context: snapshot.context.clone(), epoch: snapshot.epoch, recipient,
                sources: [directory.clone()].into(), denied_sources: [directory.clone()].into(), allowed: false });
        }
        editor.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 1);
        let was_restored = |actions: &ActionsBuf| actions.iter().any(|action|
            matches!(action.downcast_ref::<A2AppOp>(), Some(A2AppOp::RestoreAgentDefaults(id)) if id.as_str() == room));
        for (index, is_covered) in [(0, true), (1, false)] {
            editor.decision_selection = Some(decision_key(&editor.decisions[index]));
            editor.update_diagnostic_details(&mut cx);
            assert!(editor.view.widget(&cx, ids!(decision_restore_section)).visible(), "every blocked agent request offers it");
            let text = editor.view.label(&cx, ids!(decision_restore_text)).text();
            assert_eq!(!text.contains("don't cover the data listed above"), is_covered, "the defaults can't unblock a website: {text}");
            assert!(was_restored(&press(&mut editor, &mut cx, ids!(decision_restore))));
        }
        // A mini-app's request, or one allowed since, has nothing to restore.
        editor.decisions[1].context = ContextId::App { account: account.into(), app: "weather".into(), room: Some(room.into()) };
        editor.decisions[0].allowed = true;
        editor.decisions[0].denied_sources.clear();
        for index in [1, 0] {
            editor.decision_selection = Some(decision_key(&editor.decisions[index]));
            editor.update_diagnostic_details(&mut cx);
            assert!(!editor.view.widget(&cx, ids!(decision_restore_section)).visible());
            assert!(!was_restored(&press(&mut editor, &mut cx, ids!(decision_restore))));
        }
        editor.grants.push(SharingGrant { id: 81, source: directory.clone(), recipient: own_room.clone(),
            reader: ReaderScope::Context(snapshot.context.clone()), duration: SharingDuration::Permanent });
        editor.grants.push(SharingGrant { id: 82, source: directory, recipient: own_room,
            reader: ReaderScope::App { account: account.into(), app: "weather".into() }, duration: SharingDuration::Permanent });
        editor.decision_selection = None;
        editor.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 0);
        editor.open_card(&mut cx, &SharingCardTarget::Rule(81));
        assert!(editor.view.widget(&cx, ids!(rule_restore_section)).visible());
        assert!(was_restored(&press(&mut editor, &mut cx, ids!(rule_restore))));
        editor.open_card(&mut cx, &SharingCardTarget::Rule(82));
        assert!(!editor.view.widget(&cx, ids!(rule_restore_section)).visible(), "a mini-app's rule has no AI room defaults");
    }

    #[test]
    fn removing_a_rule_everyone_shares_or_an_ai_room_default_asks_first() {
        let account = "@alice:example.org";
        let _account = TestAccount::set(account);
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.account = account.into();
        let agent = ReaderScope::Context(context_fixture().context);
        let own = Source::Room { account: account.into(), room: "!source:example.org".into() };
        let directory = Source::RoomDirectory { account: account.into() };
        let own_room = Recipient::MatrixRoom { account: account.into(), room: "!source:example.org".into() };
        editor.grants = vec![
            SharingGrant { id: 91, source: Source::Account { account: account.into() }, recipient: Recipient::NetworkOrigin("https://example.org".into()),
                reader: ReaderScope::AllReaders, duration: SharingDuration::Permanent },
            SharingGrant { id: 92, source: directory.clone(), recipient: own_room.clone(), reader: agent.clone(), duration: SharingDuration::Permanent },
            SharingGrant { id: 93, source: directory, recipient: own_room, reader: agent.clone(), duration: SharingDuration::RobrixSession },
            SharingGrant { id: 94, source: own, recipient: Recipient::ModelProvider("model-one".into()), reader: agent, duration: SharingDuration::Permanent },
        ];
        let remove = |editor: &mut DataSharing, cx: &mut Cx, id: u64| {
            editor.saved_selection = SharingCardTarget::None;
            editor.open_card(cx, &SharingCardTarget::Rule(id));
            press(editor, cx, ids!(remove_button))
        };
        let revoked = |actions: &ActionsBuf| actions.iter().filter_map(|action| match action.downcast_ref::<A2AppOp>() {
            Some(A2AppOp::RevokeFlowSharing(id)) => Some(*id),
            _ => None,
        }).collect::<Vec<_>>();
        let confirmation = |actions: &ActionsBuf| actions.iter().find_map(|action| {
            let Some(ConfirmDeleteAction::Show(content)) = action.downcast_ref() else { return None };
            content.borrow_mut().take()
        });
        // While the AI service is unknown, any model rule may be the room's default.
        assert!(editor.model.is_none());
        for (id, title) in [(91, "Remove this shared rule?"), (92, "Remove this AI room default?"), (94, "Remove this AI room default?")] {
            let actions = remove(&mut editor, &mut cx, id);
            assert!(revoked(&actions).is_empty(), "rule {id} must be confirmed first");
            let content = confirmation(&actions).unwrap_or_else(|| panic!("rule {id} asks first"));
            assert_eq!(content.title_text, title);
            let accept = content.on_accept_clicked.unwrap();
            assert_eq!(revoked(&cx.capture_actions(|cx| accept(cx))), vec![id], "accepting removes exactly that rule");
        }
        let accept = confirmation(&remove(&mut editor, &mut cx, 91)).unwrap().on_accept_clicked.unwrap();
        {
            let _other = TestAccount::set("@bob:example.org");
            assert!(revoked(&cx.capture_actions(|cx| accept(cx))).is_empty(), "accepting after switching accounts removes nothing");
        }
        editor.model = Some(test_model("model-two"));
        for id in [93, 94] {
            assert_eq!(revoked(&remove(&mut editor, &mut cx, id)), vec![id], "rule {id} is removed at once");
        }
    }

    #[test]
    fn turn_rules_read_this_turn_only_and_never_pass_for_saved_rules() {
        let account = "@alice:example.org";
        let _account = TestAccount::set(account);
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.account = account.into();
        let snapshot = context_fixture();
        let room = "!source:example.org";
        let source = snapshot.label.first().unwrap().clone();
        editor.sources.push((source.clone(), "Private room".into()));
        editor.rooms.push((room.into(), "AI room".into()));
        editor.snapshots.push(snapshot.clone());
        editor.view.drop_down2(&cx, ids!(context_choice)).set_labels(&mut cx, vec!["Select context".into(), "Agent".into()]);
        editor.view.drop_down2(&cx, ids!(context_choice)).set_selected_item(&mut cx, 1);
        editor.grants.push(SharingGrant { id: 101, source, recipient: Recipient::NetworkOrigin("https://example.org".into()),
            reader: ReaderScope::Context(snapshot.context), duration: SharingDuration::RoomSession { account: account.into(), room: room.into() } });
        editor.turn_rules.insert(101);
        assert_eq!(editor.grant_duration_label(&editor.grants[0]), "This turn only");
        editor.open_card(&mut cx, &SharingCardTarget::Rule(101));
        assert!(editor.view.label(&cx, ids!(saved_rule_details)).text().contains("This turn only"));
        assert!(editor.removal_warning(&editor.grants[0]).is_none());
        // Building the same rule by hand would only hand back the turn's rule.
        editor.saved_selection = SharingCardTarget::None;
        editor.begin_rule(&mut cx);
        editor.view.permission_choices(&cx, ids!(recipient_kind)).set_selected_item(&mut cx, 1);
        editor.view.text_input(&cx, ids!(network_url)).set_text(&mut cx, "https://example.org");
        editor.view.permission_choices(&cx, ids!(sharing_lifetime)).set_selected_item(&mut cx, 1);
        editor.wizard_step = 2;
        editor.update_recipient_form(&mut cx);
        assert!(editor.view.label(&cx, ids!(rule_preview)).text().starts_with("This turn only"));
        assert!(!editor.view.button(&cx, ids!(add_button)).borrow().unwrap().enabled());
        assert_eq!(editor.view.button(&cx, ids!(add_button)).text(), "Allow sharing");
        editor.turn_rules.clear();
        editor.update_recipient_form(&mut cx);
        assert!(editor.view.label(&cx, ids!(rule_preview)).text().starts_with("This rule already allows:"));
    }

    #[cfg(unix)]
    #[test]
    fn turn_rules_are_the_sharing_rules_in_the_task_ledger() {
        use a2app_core::task_grants::{AppliedTask, GrantRef};
        let account = unique_account("turn-ledger");
        let _account = TestAccount::set(&account);
        let _model = TestModel::set(Err("No AI service in this test.".into()));
        super::super::runtime::initialize_background_test(a2app_core::builtin::stock("account").unwrap());
        let room = "!turn-ledger-ai:example.org";
        let context = crate::a2app::information_flow::prepare_agent(room).unwrap();
        let source = Source::Room { account: account.clone(), room: room.into() };
        let website = Recipient::NetworkOrigin("https://turn-ledger.example".into());
        let reader = ReaderScope::Context(context.clone());
        let turn = flow::grant_sharing(source.clone(), website.clone(), reader.clone(),
            SharingDuration::RoomSession { account: account.clone(), room: room.into() }).unwrap();
        let kept = flow::grant_sharing(source, website, reader, SharingDuration::RobrixSession).unwrap();
        // The turn's other grants may reuse a sharing rule's id; only its Flow grants are sharing rules.
        with_a2app(|state| state.task_ledger.insert(AppliedTask {
            task_id: 1, subject: a2app_core::permissions::agent_subject(room), context, epoch: 0, title: "Summarize".into(), plan_hash: [0; 32],
            grants: vec![GrantRef::Scoped(kept), GrantRef::Flow(turn), GrantRef::Network(kept), GrantRef::Tool(kept)], item_states: Default::default(),
        })).unwrap();
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.configure(&mut cx);
        assert_eq!(editor.turn_rules, [turn].into());
        let label = |editor: &DataSharing, id: u64| editor.grants.iter().find(|grant| grant.id == id).map(|grant| editor.grant_duration_label(grant)).unwrap();
        assert_eq!(label(&editor, turn), "This turn only");
        assert_eq!(label(&editor, kept), "Until Robrix closes");
        with_a2app(|state| state.task_ledger.remove(1)).unwrap();
        editor.refresh_rules(&mut cx);
        assert!(editor.turn_rules.is_empty(), "the label ends with the turn");
        assert!(label(&editor, turn).starts_with("Until "), "{}", label(&editor, turn));
        flow::revoke_sharing(turn).unwrap();
        flow::revoke_sharing(kept).unwrap();
    }

    #[test]
    fn history_points_blocked_checks_to_needs_attention_and_can_reopen_them() {
        use a2app_core::protection_audit::{self, Activity, ActivityKind, ActivityOutcome};
        let account = unique_account("history-route");
        let _account = TestAccount::set(&account);
        let _model = TestModel::set(Err("No AI service in this test.".into()));
        let room = "!history-route-ai:example.org";
        let context = crate::a2app::information_flow::prepare_agent(room).unwrap();
        let own = Source::Room { account: account.clone(), room: room.into() };
        let directory = Source::RoomDirectory { account: account.clone() };
        flow::add_sources(&context, [directory.clone()]).unwrap();
        let website = Recipient::NetworkOrigin("https://history-route.example".into());
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.configure(&mut cx);
        assert!(editor.decisions.is_empty(), "an earlier run's check is only in History");
        editor.activities = vec![Activity { id: 7, timestamp_ms: 1, account: account.clone(), context: Some(context.clone()),
            recipient: Some(website.clone()), sources: [own.clone(), directory.clone()].into(), blocked_sources: [own.clone(), directory.clone()].into(),
            kind: ActivityKind::SharingCheck, outcome: ActivityOutcome::Blocked }];
        editor.activity_key = Some((protection_audit::revision(), 0, None, None, None));
        editor.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 2);
        editor.activity_selection = Some(7);
        editor.refresh_activity(&mut cx);
        let details = editor.view.label(&cx, ids!(activity_details)).text();
        assert!(details.contains("Review in Needs attention"));
        assert!(!details.contains("Review sharing rule") && !details.contains("Allow sharing"), "History has neither button: {details}");
        assert!(editor.view.widget(&cx, ids!(history_review)).visible());
        let actions = press(&mut editor, &mut cx, ids!(history_review));
        assert!(!actions.iter().any(|action| action.downcast_ref::<A2AppOp>().is_some()), "opening a request approves nothing");
        assert_eq!(editor.view.permission_choices(&cx, ids!(page_choice)).selected_item(), 1);
        assert_eq!(editor.decision_selection, Some((context.clone(), website.clone())));
        assert_eq!(editor.blocked_sources, vec![own.clone(), directory.clone()]);
        editor.refresh_diagnostics(&mut cx);
        assert!(editor.selected_decision(&cx).is_some(), "it stays listed until it is resolved");
        let reader = ReaderScope::Context(context);
        for source in [own, directory] {
            flow::grant_sharing(source, website.clone(), reader.clone(), SharingDuration::RobrixSession).unwrap();
        }
        editor.activity_selection = Some(7);
        editor.refresh_activity(&mut cx);
        assert!(editor.view.label(&cx, ids!(activity_details)).text().contains("Allowed now. Send your message again."));
        assert!(!editor.view.widget(&cx, ids!(history_review)).visible());
    }

    #[test]
    fn history_can_allow_data_the_agent_no_longer_holds_without_calling_it_allowed() {
        use a2app_core::protection_audit::{self, Activity, ActivityKind, ActivityOutcome};
        let account = unique_account("history-unheld");
        let _account = TestAccount::set(&account);
        let _model = TestModel::set(Err("No AI service in this test.".into()));
        let room = "!history-unheld-ai:example.org";
        // As after a restart: the new session's label holds only the agent's own room.
        let context = crate::a2app::information_flow::prepare_agent(room).unwrap();
        let other = Source::Room { account: account.clone(), room: "!history-unheld-other:example.org".into() };
        let third = Source::Room { account: account.clone(), room: "!history-unheld-third:example.org".into() };
        let website = Recipient::NetworkOrigin("https://history-unheld.example".into());
        let (mut cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.configure(&mut cx);
        let record = |id: u64, source: &Source| Activity { id, timestamp_ms: id, account: account.clone(), context: Some(context.clone()),
            recipient: Some(website.clone()), sources: [source.clone()].into(), blocked_sources: [source.clone()].into(),
            kind: ActivityKind::SharingCheck, outcome: ActivityOutcome::Blocked };
        editor.activities = vec![record(9, &other), record(10, &third)];
        editor.activity_key = Some((protection_audit::revision(), 0, None, None, None));
        editor.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 2);
        editor.activity_selection = Some(9);
        editor.refresh_activity(&mut cx);
        let details = editor.view.label(&cx, ids!(activity_details)).text();
        assert!(details.contains("The agent no longer holds this data") && details.contains("choose Review in Needs attention"), "{details}");
        assert!(!details.contains("Allowed now"), "no rule allows it yet: {details}");
        assert!(editor.view.widget(&cx, ids!(history_review)).visible(), "it can still be allowed ahead of time");
        press(&mut editor, &mut cx, ids!(history_review));
        assert_eq!(editor.decision_selection, Some((context.clone(), website.clone())));
        editor.refresh_diagnostics(&mut cx);
        assert_eq!(editor.blocked_sources, vec![other.clone()], "a reopened request keeps data the agent no longer holds");
        assert!(editor.view.widget(&cx, ids!(review_section)).visible());
        assert!(editor.view.widget(&cx, ids!(allow_all_section)).visible());
        // Reopening another record of the same request adds its data.
        editor.decision_selection = None;
        editor.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 2);
        editor.activity_selection = Some(10);
        editor.refresh_activity(&mut cx);
        press(&mut editor, &mut cx, ids!(history_review));
        assert_eq!(editor.blocked_sources, vec![other.clone(), third.clone()]);
        for source in [other, third] {
            flow::grant_sharing(source, website.clone(), ReaderScope::Context(context.clone()), SharingDuration::RobrixSession).unwrap();
        }
        editor.refresh_diagnostics(&mut cx);
        assert!(editor.view.label(&cx, ids!(decision_details)).text().contains("Allowed now. Send your message again."));
    }

}
