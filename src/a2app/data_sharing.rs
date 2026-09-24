//! Source sharing rules, separate from permission to read or contact a service.
//!
//! Changes go through the runtime so revocation also cancels active agents.

use makepad_widgets::*;
use a2app_core::information_flow::{
    self as flow, ActionAuthority, ActionDecision, AuthoritySession, ContextId, ContextSnapshot,
    FlowDecision, Influence, ReaderScope, Recipient, SharingDuration, SharingGrant, Source,
};
use a2app_agent::model_transport::ModelRecipient;
use crate::home::rooms_list::RoomsListRef;
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
                SubsectionLabel { text: "Where private data can go", margin: 0 }
                PermissionOptionLabel { text: "Mini-apps and agents need a sharing rule before sending private room or account data to another destination." }
                new_rule := RobrixPositiveIconButton { text: "Add sharing rule", padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0} }
                current_rules := PermissionOptionLabel {}
                sharing_error := PermissionOptionLabel { visible: false, draw_text +: { color: (COLOR_FG_DANGER_RED) } }
                rules_list := SharingList {}
                PermissionOptionLabel { text: "Removing a rule stops future sharing. It cannot recall data already sent." }
            }
            rule_details_page := View {
                visible: false, width: Fill, height: Fit, flow: Down, spacing: 12
                rule_back := TextButton { text: "Back to rules" }
                SubsectionLabel { text: "Sharing permission", margin: 0 }
                saved_rule_details := PermissionOptionLabel {}
                saved_identifiers_toggle := TextButton { text: "Show identifiers" }
                saved_identifiers := PermissionOptionLabel { visible: false }
                remove_button := RobrixNegativeIconButton { text: "Remove this permission", padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0} }
                PermissionOptionLabel { text: "Removing permission stops future requests and cancels active private-data work. It cannot recall anything already sent." }
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
                        context_choice := PermissionDropDown { labels: ["Choose a running mini-app or agent"] }
                    }
                    reader_app_section := View { visible: false, width: Fill, height: Fit, flow: Down, reader_app := PermissionDropDown {} }
                    reader_preview := PermissionOptionLabel {}
                    more_readers := TextButton { text: "More sharing options" }
                    reader_options := View {
                        visible: false, width: Fill, height: Fit, flow: Down, spacing: 8
                        reader_kind := PermissionChoices { labels: ["Only this running mini-app or agent", "Everywhere this mini-app runs", "All mini-apps and agents"] }
                        PermissionOptionLabel { text: "The broader options include rooms, spaces and account-wide use. Choose them only if you want the same permission to apply everywhere." }
                    }
                    context_details := PermissionOptionLabel { visible: false }
                }
                destination_step := View {
                    visible: false, width: Fill, height: Fit, flow: Down, spacing: 10
                    recipient_kind := PermissionChoices { labels: ["My configured AI service", "A website", "Another room"] }
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
                    rule_identifiers_toggle := TextButton { text: "Show identifiers" }
                    rule_identifiers := PermissionOptionLabel { visible: false }
                    PermissionOptionLabel { text: "Allowing sharing may send private room or account data off this device to the destination shown. If several data sources are used together, each must allow that destination." }
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
                    attention_more := TextButton { visible: false, text: "Show more requests" }
                }
                attention_details := View {
                    visible: false, width: Fill, height: Fit, flow: Down, spacing: 12
                    attention_back := TextButton { text: "Back to requests" }
                    sharing_details := View {
                        visible: false, width: Fill, height: Fit, flow: Down, spacing: 12
                        SubsectionLabel { text: "Sharing was blocked", margin: 0 }
                        decision_details := PermissionOptionLabel {}
                        review_section := View {
                            width: Fill, height: Fit, flow: Down, spacing: 8
                            PermissionOptionLabel { text: "Which blocked data source do you want to review?" }
                            blocked_source_choice := PermissionChoices {}
                            review_button := TextButton { text: "Review sharing rule" }
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
                    history_filter_toggle := TextButton { text: "Filter history" }
                    history_filters := View {
                        visible: false, width: Fill, height: Fit, flow: Down
                        activity_filter := PermissionChoices { labels: ["All activity", "Blocked requests", "Network and room operations", "Settings changes"] }
                    }
                    activity_summary := PermissionOptionLabel {}
                    activity_list := SharingList {}
                    history_more := TextButton { visible: false, text: "Show more history" }
                }
                history_details := View {
                    visible: false, width: Fill, height: Fit, flow: Down, spacing: 12
                    history_back := TextButton { text: "Back to history" }
                    activity_details := PermissionOptionLabel {}
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
    #[rust] history_filters_open: bool,
    #[rust] saved_selection: SharingCardTarget,
    #[rust] decision_selection: usize,
    #[rust] action_selection: usize,
    #[rust] activity_selection: Option<u64>,
    #[rust(20)] attention_limit: usize,
    #[rust(20)] history_limit: usize,
    #[rust] sources: Vec<(Source, String)>,
    #[rust] rooms: Vec<(String, String)>,
    #[rust] model: Option<ModelRecipient>,
    #[rust] apps: Vec<(String, String)>,
    #[rust] snapshots: Vec<ContextSnapshot>,
    #[rust] grants: Vec<SharingGrant>,
    #[rust] decisions: Vec<FlowDecision>,
    #[rust] blocked_sources: Vec<Source>,
    #[rust] action_decisions: Vec<ActionDecision>,
    #[rust] authorities: Vec<ActionAuthority>,
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
            self.decision_selection = 0;
            self.action_selection = 0;
            self.activity_selection = None;
            self.saved_selection = SharingCardTarget::None;
            self.update_page(cx);
            return;
        }
        for action in actions {
            if let Some(action) = action.downcast_ref::<SharingCardAction>()
                && action.owner == self.widget_uid() && action.account == self.account && !self.wizard_open
                && match &action.target {
                    SharingCardTarget::Rule(_) | SharingCardTarget::Authority(_) => page == 0 && matches!(self.saved_selection, SharingCardTarget::None),
                    SharingCardTarget::Decision(_) | SharingCardTarget::Action(_) => page == 1 && self.decision_selection == 0 && self.action_selection == 0,
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
            self.update_page(cx);
            return;
        }
        if self.wizard_open && self.wizard_step == 0 && self.view.button(cx, ids!(more_readers)).clicked(actions) {
            self.reader_options_open = !self.reader_options_open;
            self.update_recipient_form(cx);
        }
        if self.wizard_open && self.wizard_step == 2 && self.view.button(cx, ids!(rule_identifiers_toggle)).clicked(actions) {
            self.rule_identifiers_open = !self.rule_identifiers_open;
            self.update_recipient_form(cx);
        }
        if page == 0 && !self.wizard_open && !matches!(self.saved_selection, SharingCardTarget::None)
            && self.view.button(cx, ids!(saved_identifiers_toggle)).clicked(actions)
        {
            self.saved_identifiers_open = !self.saved_identifiers_open;
            self.update_saved_details(cx);
        }
        if page == 0 && !self.wizard_open && self.view.button(cx, ids!(rule_back)).clicked(actions) {
            self.saved_selection = SharingCardTarget::None;
            self.update_page(cx);
            return;
        }
        if page == 1 && !self.wizard_open && self.view.button(cx, ids!(attention_back)).clicked(actions) {
            self.decision_selection = 0;
            self.action_selection = 0;
            self.update_page(cx);
            return;
        }
        if page == 2 && !self.wizard_open && self.view.button(cx, ids!(history_back)).clicked(actions) {
            self.activity_selection = None;
            self.update_page(cx);
            return;
        }
        if self.view.button(cx, ids!(attention_more)).clicked(actions) { self.attention_limit += 20; }
        if self.view.button(cx, ids!(history_more)).clicked(actions) { self.history_limit += 20; }
        if self.view.button(cx, ids!(history_filter_toggle)).clicked(actions) {
            self.history_filters_open = !self.history_filters_open;
            self.view.widget(cx, ids!(history_filters)).set_visible(cx, self.history_filters_open);
        }
        if self.view.permission_choices(cx, ids!(activity_filter)).changed(actions).is_some() {
            self.activity_selection = None;
            self.history_limit = 20;
        }
        if self.view.drop_down(cx, ids!(source_choice)).changed(actions).is_some()
            || self.view.drop_down(cx, ids!(context_choice)).changed(actions).is_some()
            || self.view.drop_down(cx, ids!(reader_app)).changed(actions).is_some()
            || self.view.drop_down(cx, ids!(target_room)).changed(actions).is_some()
            || self.view.drop_down(cx, ids!(lifetime_room)).changed(actions).is_some()
            || self.view.text_input(cx, ids!(network_url)).changed(actions).is_some()
            || self.view.permission_choices(cx, ids!(reader_kind)).changed(actions).is_some()
            || self.view.permission_choices(cx, ids!(recipient_kind)).changed(actions).is_some()
            || self.view.permission_choices(cx, ids!(sharing_lifetime)).changed(actions).is_some()
            || self.view.permission_choices(cx, ids!(action_session)).changed(actions).is_some()
        {
            self.update_diagnostic_details(cx);
            self.update_recipient_form(cx);
        }
        if page == 1 && !self.wizard_open && self.decision_selection != 0 && self.view.button(cx, ids!(review_button)).clicked(actions) {
            if let Err(error) = self.review_decision(cx) { self.show_error(error); }
            return;
        }
        if self.view.button(cx, ids!(add_button)).clicked(actions) && self.wizard_open && self.wizard_step == 2 {
            let result = self.sharing_action(cx);
            self.submit(cx, result);
        }
        if page == 0 && !self.wizard_open && self.view.button(cx, ids!(remove_button)).clicked(actions) {
            let result = match self.saved_selection {
                SharingCardTarget::Rule(id) if self.grants.iter().any(|grant| grant.id == id) => Ok(A2AppOp::RevokeFlowSharing(id)),
                SharingCardTarget::Authority(id) if self.authorities.iter().any(|grant| grant.id == id) => Ok(A2AppOp::RevokeFlowAuthority(id)),
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
        self.refresh_diagnostics(cx);
        self.refresh_rules(cx);
        self.refresh_activity(cx);
        self.update_recipient_form(cx);
        self.update_saved_details(cx);
        let lists = [ids!(rules_list), ids!(attention_list), ids!(activity_list)]
            .map(|path| self.view.widget(cx, path).widget_uid());
        while let Some(subview) = self.view.draw_walk(cx, scope, walk).step() {
            let uid = subview.widget_uid();
            if let Some(mut list) = subview.as_flat_list().borrow_mut()
                && let Some(kind) = lists.iter().position(|candidate| *candidate == uid)
            {
                self.draw_cards(cx, &mut list, kind);
            }
        }
        DrawStep::done()
    }
}

impl DataSharing {
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
                1 if self.decision_selection != 0 || self.action_selection != 0 => {
                    self.decision_selection = 0;
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
        let page = self.view.permission_choices(cx, ids!(page_choice)).selected_item();
        let saved = !matches!(self.saved_selection, SharingCardTarget::None);
        self.view.widget(cx, ids!(tabs)).set_visible(cx, !self.wizard_open);
        self.view.widget(cx, ids!(rules_page)).set_visible(cx, page == 0 && !saved && !self.wizard_open);
        self.view.widget(cx, ids!(rule_details_page)).set_visible(cx, page == 0 && saved && !self.wizard_open);
        self.view.widget(cx, ids!(wizard)).set_visible(cx, self.wizard_open);
        self.view.widget(cx, ids!(review_page)).set_visible(cx, page == 1 && !self.wizard_open);
        self.view.widget(cx, ids!(activity_page)).set_visible(cx, page == 2 && !self.wizard_open);
        let reviewing = self.decision_selection != 0 || self.action_selection != 0;
        self.view.widget(cx, ids!(attention_overview)).set_visible(cx, !reviewing);
        self.view.widget(cx, ids!(attention_details)).set_visible(cx, reviewing);
        self.view.widget(cx, ids!(sharing_details)).set_visible(cx, self.decision_selection != 0);
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
            SharingCardTarget::Decision(decision) => {
                let Some(index) = self.decisions.iter().position(|current| current == decision) else { return };
                self.decision_selection = index + 1;
                self.action_selection = 0;
            }
            SharingCardTarget::Action(decision) => {
                let Some(index) = self.action_decisions.iter().position(|current| current == decision) else { return };
                self.action_selection = index + 1;
                self.decision_selection = 0;
                self.view.permission_choices(cx, ids!(action_session)).set_selected_item(cx, 0);
            }
            SharingCardTarget::Activity(id) if self.activities.iter().any(|entry| entry.id == *id) => self.activity_selection = Some(*id),
            _ => return,
        }
        self.saved_identifiers_open = false;
        self.update_diagnostic_details(cx);
        self.update_saved_details(cx);
        self.update_page(cx);
    }

    fn update_saved_details(&mut self, cx: &mut Cx) {
        let details = match self.saved_selection {
            SharingCardTarget::Rule(id) => self.grants.iter().find(|grant| grant.id == id).map(|grant| (
                self.sharing_rule_summary(&grant.source, &grant.reader, &grant.recipient, &grant.duration),
                format!("Data: {}\n\nWho can share it: {}\n\nDestination: {}\n\nFor how long: {}", self.source_label(&grant.source), self.reader_label(&grant.reader), self.recipient_label(&grant.recipient), self.duration_label(&grant.duration)))),
            SharingCardTarget::Authority(id) => self.authorities.iter().find(|grant| grant.id == id).map(|grant| (
                format!("{}\n{}\n{}\n\nAccount: {}", self.context_choice_label(&grant.context), self.action_choice_label(&grant.action), match &grant.session {
                    AuthoritySession::RoomSession { room, .. } => format!("Until {} closes", self.room_name(room)),
                    session => self.authority_duration_label(session),
                }, self.account),
                format!("{}\n\nAction: {}\nTarget: {}\n\n{}", self.context_label(&grant.context), grant.action.kind, grant.action.target, self.authority_duration_label(&grant.session)))),
            _ => None,
        };
        self.view.label(cx, ids!(saved_rule_details)).set_text(cx, details.as_ref().map(|(summary, _)| summary.as_str()).unwrap_or("This permission is no longer saved."));
        self.view.label(cx, ids!(saved_identifiers)).set_text(cx, details.as_ref().map(|(_, exact)| exact.as_str()).unwrap_or_default());
        self.view.widget(cx, ids!(saved_identifiers_toggle)).set_visible(cx, details.is_some());
        self.view.button(cx, ids!(saved_identifiers_toggle)).set_text(cx, if self.saved_identifiers_open { "Hide identifiers" } else { "Show identifiers" });
        self.view.widget(cx, ids!(saved_identifiers)).set_visible(cx, details.is_some() && self.saved_identifiers_open);
        self.view.button(cx, ids!(remove_button)).set_enabled(cx, details.is_some());
    }

    fn sharing_rule_summary(&self, source: &Source, reader: &ReaderScope, recipient: &Recipient, duration: &SharingDuration) -> String {
        let destination = match recipient {
            Recipient::MatrixRoom { .. } => self.recipient_choice_label(recipient),
            _ => self.recipient_label(recipient),
        };
        let duration = match duration {
            SharingDuration::RoomSession { room, .. } => format!("Until {} closes", self.room_name(room)),
            duration => self.duration_label(duration),
        };
        format!("{} can share\n{}\n\nWith: {destination}\nFor how long: {duration}\n\nAccount: {}", self.reader_choice_label(reader), self.source_choice_label(source), self.account)
    }

    fn draw_cards(&mut self, cx: &mut Cx2d, list: &mut FlatList, kind: usize) {
        let mut rows: Vec<(String, String, SharingCardTarget, &str)> = Vec::new();
        match kind {
            0 => {
                rows.extend(self.grants.iter().map(|grant| (
                    format!("{} → {}", self.source_choice_label(&grant.source), self.recipient_choice_label(&grant.recipient)),
                    format!("{}\n{}", self.reader_choice_label(&grant.reader), self.duration_label(&grant.duration)), SharingCardTarget::Rule(grant.id), "View rule")));
                rows.extend(self.authorities.iter().map(|grant| (
                    self.action_choice_label(&grant.action), format!("{}\n{}", self.context_choice_label(&grant.context), self.authority_duration_label(&grant.session)),
                    SharingCardTarget::Authority(grant.id), "View approval")));
            }
            1 => {
                rows.extend(self.decisions.iter().filter(|decision| !decision.allowed).take(self.attention_limit).map(|decision| (
                    format!("Sharing with {}", self.recipient_choice_label(&decision.recipient)),
                    format!("{}\nBlocked at its last check. Review the destination and data before allowing sharing.", self.context_choice_label(&decision.context)),
                    SharingCardTarget::Decision(decision.clone()), "Review sharing")));
                let remaining = self.attention_limit.saturating_sub(rows.len());
                rows.extend(self.action_decisions.iter().take(remaining).map(|decision| (
                    self.action_choice_label(&decision.action), format!("{}\nNeeds approval before retrying.", self.context_choice_label(&decision.context)),
                    SharingCardTarget::Action(decision.clone()), "Review action")));
            }
            _ => rows.extend(self.activities.iter().take(self.history_limit).map(|entry| (
                format!("{} · {}", activity::activity_outcome(entry.outcome), activity::activity_kind(entry.kind)),
                format!("{}\n{}", activity::activity_time(entry.timestamp_ms), entry.context.as_ref().map(|context| self.context_choice_label(context)).unwrap_or_else(|| "Account settings".into())),
                SharingCardTarget::Activity(entry.id), "View details"))),
        }
        for (index, (title, summary, target, button)) in rows.into_iter().enumerate() {
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
        let previous_app = self.apps.get(self.view.drop_down(cx, ids!(reader_app)).selected_item()).map(|(id, _)| id.clone());
        let previous_target = self.rooms.get(self.view.drop_down(cx, ids!(target_room)).selected_item()).map(|(id, _)| id.clone());
        let previous_expiry = self.rooms.get(self.view.drop_down(cx, ids!(lifetime_room)).selected_item()).map(|(id, _)| id.clone());
        let account = super::information_flow::account().unwrap_or_default();
        let account_changed = self.account != account;
        self.account = account;
        if account_changed {
            self.wizard_open = false;
            self.wizard_step = 0;
            self.reader_options_open = false;
            self.rule_identifiers_open = false;
            self.saved_identifiers_open = false;
            self.saved_selection = SharingCardTarget::None;
            self.decision_selection = 0;
            self.action_selection = 0;
            self.activity_selection = None;
            for id in [ids!(reader_kind), ids!(sharing_lifetime), ids!(recipient_kind), ids!(page_choice)] {
                self.view.permission_choices(cx, id).set_selected_item(cx, 0);
            }
            self.view.text_input(cx, ids!(network_url)).set_text(cx, "");
        }
        self.rooms = if cx.has_global::<RoomsListRef>() {
            cx.get_global::<RoomsListRef>().permission_targets().into_iter()
                .filter(|(_, _, space)| !space).map(|(id, name, _)| (id, name)).collect()
        } else { Vec::new() };
        self.apps = with_a2app(|state| state.registry.iter().map(|app| (app.id.clone(), app.name.clone())).collect()).unwrap_or_default();
        self.view.drop_down(cx, ids!(reader_app)).set_labels(cx, self.apps.iter().map(|(_, name)| name.clone()).collect());
        self.view.drop_down(cx, ids!(reader_app)).set_selected_item(cx,
            previous_app.filter(|_| !account_changed).and_then(|app| self.apps.iter().position(|(id, _)| id == &app)).unwrap_or(0));
        self.view.drop_down(cx, ids!(lifetime_room)).set_labels(cx, self.rooms.iter().map(|(_, name)| name.clone()).collect());
        self.view.drop_down(cx, ids!(lifetime_room)).set_selected_item(cx,
            previous_expiry.filter(|_| !account_changed).and_then(|room| self.rooms.iter().position(|(id, _)| id == &room)).unwrap_or(0));
        self.sources.clear();
        if !self.account.is_empty() {
            self.sources.push((Source::Account { account: self.account.clone() }, format!("Account data · {}", self.account)));
            self.sources.extend(self.rooms.iter().map(|(room, name)| (
                Source::Room { account: self.account.clone(), room: room.clone() }, format!("Room · {name} ({room})"),
            )));
            // Previously configured sources remain revocable after leaving a room.
            if let Ok(grants) = flow::sharing_grants() {
                for source in grants.into_iter().map(|grant| grant.source) {
                    if let Source::Room { account, room } = &source
                        && account == &self.account && !self.sources.iter().any(|(candidate, _)| candidate == &source)
                    {
                        self.sources.push((source.clone(), format!("Earlier room · {room}")));
                    }
                }
            }
        }
        self.view.drop_down(cx, ids!(source_choice)).set_labels(cx, self.sources.iter().map(|(source, _)| self.source_choice_label(source)).collect());
        self.view.drop_down(cx, ids!(source_choice)).set_selected_item(cx,
            previous_source.filter(|_| !account_changed).and_then(|source| self.sources.iter().position(|(candidate, _)| candidate == &source)).unwrap_or(0));
        self.view.drop_down(cx, ids!(target_room)).set_labels(cx, self.rooms.iter().map(|(_, name)| name.clone()).collect());
        self.view.drop_down(cx, ids!(target_room)).set_selected_item(cx,
            previous_target.filter(|_| !account_changed).and_then(|room| self.rooms.iter().position(|(id, _)| id == &room)).unwrap_or(0));
        let prefs = with_a2app(|state| state.agent_prefs.clone()).unwrap_or_else(a2app_agent::prefs::load_agent_prefs);
        match a2app_agent::model_transport::current_recipient(&prefs) {
            Ok(model) => {
                let location = if model.local { "Local endpoint" } else { "Online service" };
                self.view.label(cx, ids!(model_description)).set_text(cx, &format!("{}\n{location}: {}", model.label, model.endpoint));
                self.model = Some(model);
            }
            Err(error) => {
                self.model = None;
                self.view.label(cx, ids!(model_description)).set_text(cx, &format!("{error}\nConfigure a supported provider in Setup AI Providers."));
            }
        }
        self.refresh_diagnostics(cx);
        self.update_recipient_form(cx);
        self.refresh_rules(cx);
        self.update_page(cx);
        self.view.redraw(cx);
    }

    fn selected_source(&self, cx: &Cx) -> Result<Source, String> {
        self.sources.get(self.view.drop_down(cx, ids!(source_choice)).selected_item())
            .map(|(source, _)| source.clone()).ok_or_else(|| "Sign in and select a data source.".into())
    }

    fn selected_recipient(&self, cx: &Cx) -> Result<Recipient, String> {
        match self.view.permission_choices(cx, ids!(recipient_kind)).selected_item() {
            0 => self.model.as_ref().map(|model| Recipient::ModelProvider(model.id.clone()))
                .ok_or_else(|| "Configure a supported AI model service first.".into()),
            1 => Recipient::network_origin(self.view.text_input(cx, ids!(network_url)).text().trim()),
            2 => self.rooms.get(self.view.drop_down(cx, ids!(target_room)).selected_item())
                .map(|(room, _)| Recipient::MatrixRoom { account: self.account.clone(), room: room.clone() })
                .ok_or_else(|| "Select a joined destination room.".into()),
            _ => Err("Select a recipient type.".into()),
        }
    }

    fn update_recipient_form(&mut self, cx: &mut Cx) {
        let kind = self.view.permission_choices(cx, ids!(recipient_kind)).selected_item();
        self.view.widget(cx, ids!(model_section)).set_visible(cx, kind == 0);
        self.view.widget(cx, ids!(network_section)).set_visible(cx, kind == 1);
        self.view.widget(cx, ids!(room_section)).set_visible(cx, kind == 2);
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
        self.view.button(cx, ids!(more_readers)).set_text(cx, if self.reader_options_open { "Hide sharing options" } else { "More sharing options" });
        let room_lifetime = self.view.permission_choices(cx, ids!(sharing_lifetime)).selected_item() == 1;
        self.view.widget(cx, ids!(lifetime_room_section)).set_visible(cx, room_lifetime);
        let preview = self.selected_reader(cx).map(|reader| self.reader_label(&reader))
            .unwrap_or_else(|error| error);
        self.view.label(cx, ids!(reader_preview)).set_text(cx, &preview);
        let (preview, identifiers, can_allow, already_saved) = match self.sharing_action(cx) {
            Ok(A2AppOp::GrantFlowSharing { source, recipient, reader, duration }) => {
                let saved = self.grants.iter().any(|grant| grant.source == source && grant.recipient == recipient
                    && grant.reader == reader && grant.duration == duration);
                (format!("{}\n{}", if saved { "This rule already allows:" } else { "You will allow:" }, self.sharing_rule_summary(&source, &reader, &recipient, &duration)),
                format!("{}\n{}\n\nTo share:\n{}\n\nWith:\n{}\n\nFor how long: {}",
                    if saved { "This rule already allows:" } else { "You will allow:" }, self.reader_label(&reader),
                    self.source_label(&source), self.recipient_label(&recipient), self.duration_label(&duration)), !saved, saved)
            }
            Err(error) => (error, String::new(), false, false),
            _ => unreachable!("sharing_action only creates a sharing grant"),
        };
        self.view.label(cx, ids!(rule_preview)).set_text(cx, &preview);
        self.view.label(cx, ids!(rule_identifiers)).set_text(cx, &identifiers);
        self.view.widget(cx, ids!(rule_identifiers_toggle)).set_visible(cx, !identifiers.is_empty());
        self.view.button(cx, ids!(rule_identifiers_toggle)).set_text(cx, if self.rule_identifiers_open { "Hide identifiers" } else { "Show identifiers" });
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
        match flow::sharing_grants() {
            Ok(grants) => {
                self.grants = grants.into_iter().filter(|grant| match &grant.source {
                    Source::Account { account } | Source::Room { account, .. } => account == &self.account,
                    Source::UnknownPrivate => false,
                }).collect();
                self.view.widget(cx, ids!(sharing_error)).set_visible(cx, false);
                self.view.label(cx, ids!(current_rules)).set_text(cx, if self.grants.is_empty() && self.authorities.is_empty() {
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
            Ok(Source::UnknownPrivate) => "Data with unknown private sources cannot be shared.",
            _ => "Account data stays in Robrix by default. Choose exactly who can share it and where it can go.",
        };
        self.view.label(cx, ids!(source_default)).set_text(cx, hint);
    }

    fn recipient_label(&self, recipient: &Recipient) -> String {
        match recipient {
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

mod diagnostics;
mod activity;
use diagnostics::bullets;

impl DataSharingRef {
    pub fn configure(&self, cx: &mut Cx) {
        if let Some(mut inner) = self.borrow_mut() { inner.configure(cx); }
    }

    pub fn back(&self, cx: &mut Cx) -> bool {
        self.borrow_mut().is_some_and(|mut inner| inner.back(cx))
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
            crate::shared::script_mod(vm);
            super::super::permission_choices::script_mod(vm);
            super::super::permission_prompt::script_mod(vm);
            super::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.DataSharing {} });
            WidgetRef::script_from_value(vm, value)
        });
        (cx, widget)
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
        editor.view.drop_down(&cx, ids!(context_choice)).set_labels(&mut cx, vec!["Choose an app".into(), "This agent".into()]);
        editor.view.drop_down(&cx, ids!(context_choice)).set_selected_item(&mut cx, 1);
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
        let actions = click(&mut editor, &mut cx, ids!(rule_identifiers_toggle));
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
        editor.view.drop_down(&cx, ids!(context_choice)).set_labels(&mut cx, vec!["Select context".into(), "Fixture context".into()]);
        editor.view.drop_down(&cx, ids!(context_choice)).set_selected_item(&mut cx, 1);
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
        editor.view.drop_down(&cx, ids!(context_choice)).set_labels(&mut cx, vec!["Select context".into(), "Fixture context".into()]);
        editor.view.drop_down(&cx, ids!(context_choice)).set_selected_item(&mut cx, 1);
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
        editor.view.drop_down(&cx, ids!(context_choice)).set_labels(&mut cx, vec!["Select context".into(), "Fixture context".into()]);
        editor.view.drop_down(&cx, ids!(context_choice)).set_selected_item(&mut cx, 1);
        let source = snapshot.label.first().unwrap().clone();
        editor.decisions.push(FlowDecision {
            context: snapshot.context.clone(), epoch: snapshot.epoch, recipient: Recipient::NetworkOrigin("https://blocked.example:8443".into()),
            sources: snapshot.label.clone(), denied_sources: snapshot.label, allowed: false,
        });

        editor.decision_selection = 1;
        editor.update_diagnostic_details(&mut cx);
        editor.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 1);
        editor.update_page(&mut cx);
        editor.review_decision(&mut cx).unwrap();
        assert_eq!(editor.view.permission_choices(&cx, ids!(page_choice)).selected_item(), 0);
        assert_eq!(editor.selected_source(&cx).unwrap(), source);
        assert_eq!(editor.selected_reader(&cx).unwrap(), ReaderScope::Context(snapshot.context));
        assert_eq!(editor.selected_recipient(&cx).unwrap(), Recipient::NetworkOrigin("https://blocked.example:8443".into()));
        editor.decisions[0].recipient = Recipient::ModelProvider("stale-model-identity".into());
        assert!(editor.review_decision(&mut cx).is_err());
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
        editor.decision_selection = 1;
        for step in [1, 0] {
            assert!(editor.back(&mut cx));
            assert_eq!(editor.wizard_step, step);
            assert!(editor.wizard_open);
        }
        assert!(editor.back(&mut cx));
        assert!(!editor.wizard_open);
        assert_eq!(editor.view.permission_choices(&cx, ids!(page_choice)).selected_item(), 1);
        assert_eq!(editor.decision_selection, 1, "leaving a rule returns to the request that opened it");
        assert!(editor.back(&mut cx));
        assert_eq!(editor.decision_selection, 0);
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
        assert!(text.contains("Sharing rules"));
        assert!(text.contains("Review sharing rule"));
        assert!(text.contains("Allow sharing"));
        assert!(editor.sharing_remedy(&[Source::UnknownPrivate].into(), &recipient).contains("No sharing toggle"));
    }

}
