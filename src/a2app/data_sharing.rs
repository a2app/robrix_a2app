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

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    mod.widgets.DataSharing = set_type_default() do #(DataSharing::register_widget(vm)) {
        width: Fill, height: Fill, flow: Down
        content := ScrollYView {
            width: Fill, height: Fill, flow: Down, spacing: 10, padding: 15
            mod.widgets.PermissionOptionLabel {
                text: "Control where room and account data may be sent by any mini-app or agent. Reading data and granting internet access do not grant permission to share private data."
            }
            mod.widgets.PermissionOptionLabel {
                text: "Every accumulated source must allow the actual recipient. Choose an app or agent and a lifetime for each rule. Rules never remove a source from stored data or history."
            }
            SubsectionLabel { text: "App or agent context", margin: 0 }
            context_choice := mod.widgets.PermissionDropDown {labels: ["Select an app or agent context"]}
            context_details := mod.widgets.PermissionOptionLabel {}
            SubsectionLabel { text: "Recent sharing decisions", margin: 0 }
            decision_choice := mod.widgets.PermissionDropDown {labels: ["Select a sharing decision"]}
            decision_details := mod.widgets.PermissionOptionLabel {}
            review_section := View {
                width: Fill, height: Fit, flow: Down, spacing: 7
                blocked_source_choice := mod.widgets.PermissionDropDown {}
                review_button := RobrixNeutralIconButton {
                    padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}
                    text: "Review this source and recipient below"
                }
            }
            mod.widgets.PermissionOptionLabel {
                text: "This history contains source identities, recipients and decisions only, never message bodies, request payloads, model text or credentials."
            }
            SubsectionLabel { text: "Protection activity", margin: 0 }
            activity_filter := mod.widgets.PermissionDropDown {
                labels: ["All activity in this account", "Selected app or agent context", "Selected data source", "Selected recipient"]
            }
            activity_choice := mod.widgets.PermissionDropDown { labels: ["Select an activity record"] }
            activity_details := mod.widgets.PermissionOptionLabel {}
            activity_warning := mod.widgets.PermissionOptionLabel { visible: false }
            mod.widgets.PermissionOptionLabel {
                text: "Recent metadata is saved on this device across restarts. A policy allowance is not proof that a request was sent. Failed or interrupted requests may already have transmitted data."
            }
            SubsectionLabel { text: "Data source", margin: 0 }
            source_choice := mod.widgets.PermissionDropDown {}
            source_default := mod.widgets.PermissionOptionLabel {}
            sharing_error := mod.widgets.PermissionOptionLabel {
                visible: false
                draw_text +: { color: (COLOR_FG_DANGER_RED) }
            }
            SubsectionLabel { text: "Allowed recipients", margin: 0 }
            current_rules := mod.widgets.PermissionOptionLabel {}
            remove_section := View {
                width: Fill, height: Fit, flow: Down, spacing: 7
                remove_choice := mod.widgets.PermissionDropDown {}
                remove_button := RobrixNegativeIconButton {
                    padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}
                    text: "Remove selected sharing rule"
                }
            }
            SubsectionLabel { text: "Who may share this source?", margin: 0 }
            reader_kind := mod.widgets.PermissionDropDown {
                labels: ["Selected app or agent context", "One mini-app across its rooms", "All mini-apps and agents"]
            }
            reader_app_section := View {
                visible: false, width: Fill, height: Fit, flow: Down
                reader_app := mod.widgets.PermissionDropDown {}
            }
            reader_preview := mod.widgets.PermissionOptionLabel {}
            sharing_lifetime := mod.widgets.PermissionDropDown {
                labels: ["Until Robrix closes", "Until a room closes", "Always"]
            }
            lifetime_room_section := View {
                visible: false, width: Fill, height: Fit, flow: Down
                lifetime_room := mod.widgets.PermissionDropDown {}
            }
            SubsectionLabel { text: "Allow a recipient", margin: 0 }
            recipient_kind := mod.widgets.PermissionDropDown {
                labels: ["Configured AI model service", "Website origin", "Another Matrix room"]
            }
            model_section := View {
                width: Fill, height: Fit, flow: Down, spacing: 5
                model_description := mod.widgets.PermissionOptionLabel {}
                mod.widgets.PermissionOptionLabel {
                    text: "Model requests include conversation history and tool results. This rule is tied to the current service, endpoint, model and account credentials; changing them requires a new rule."
                }
            }
            network_section := View {
                visible: false
                width: Fill, height: Fit, flow: Down, spacing: 5
                network_url := RobrixTextInput { width: Fill, empty_text: "https://example.com" }
                network_preview := mod.widgets.PermissionOptionLabel {}
                mod.widgets.PermissionOptionLabel {
                    text: "Allows sharing with every path on this exact origin (scheme, host and port). Other origins, including redirect destinations, need their own rules."
                }
            }
            room_section := View {
                visible: false
                width: Fill, height: Fit, flow: Down, spacing: 5
                target_room := mod.widgets.PermissionDropDown {}
                mod.widgets.PermissionOptionLabel {
                    text: "Allows data from the selected source to be included in messages and other operations in this destination room. Normal room permissions still apply."
                }
            }
            add_button := RobrixPositiveIconButton {
                padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}
                text: "Allow sharing with this recipient"
            }
            SubsectionLabel { text: "Sensitive actions", margin: 0 }
            mod.widgets.PermissionOptionLabel {
                text: "Reading room content, websites, other mini-apps or model replies does not authorize their instructions. Review a blocked action before allowing that exact action and target for this context. New influences require another review."
            }
            action_choice := mod.widgets.PermissionDropDown {labels: ["Select a blocked sensitive action"]}
            action_details := mod.widgets.PermissionOptionLabel {}
            action_session := mod.widgets.PermissionDropDown {
                labels: ["This exact action once", "Until this context's room closes", "Until Robrix closes"]
            }
            authority_button := RobrixPositiveIconButton {
                padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}
                text: "Allow this exact action once"
            }
            mod.widgets.PermissionOptionLabel {
                text: "Once allows one retry with exactly the reviewed contents. It does not run the action automatically. Session permission allows repeated actions of this kind to this target, including different contents; room and sharing permissions still apply."
            }
            authority_rules := mod.widgets.PermissionOptionLabel {}
            authority_remove_section := View {
                width: Fill, height: Fit, flow: Down, spacing: 7
                authority_remove_choice := mod.widgets.PermissionDropDown {}
                authority_remove_button := RobrixNegativeIconButton {
                    padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}
                    text: "Remove selected action permission"
                }
            }
            mod.widgets.PermissionOptionLabel {
                text: "Restrictions stay with stored data, app instances and agent history after closing or restarting. Removing a rule blocks future requests; it cannot recall data already sent."
            }
            mod.widgets.PermissionOptionLabel {
                text: "Blocking read access prevents new reads. Data already read keeps its sources and sharing rules; remove the recipient rules here to stop future sharing."
            }
            mod.widgets.PermissionOptionLabel {
                text: "Older data with unknown sources cannot be shared. Restarting or opening public mode does not release that data; public instances reject private inputs."
            }
        }
    }
}

#[derive(Script, ScriptHook, Widget)]
pub struct DataSharing {
    #[deref] view: View,
    #[rust] account: String,
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
        self.view.handle_event(cx, event, scope);
        let Event::Actions(actions) = event else { return };
        if self.view.drop_down(cx, ids!(source_choice)).changed(actions).is_some() {
            self.refresh_rules(cx);
        }
        if self.view.drop_down(cx, ids!(recipient_kind)).changed(actions).is_some()
            || self.view.text_input(cx, ids!(network_url)).changed(actions).is_some()
            || self.view.drop_down(cx, ids!(reader_kind)).changed(actions).is_some()
            || self.view.drop_down(cx, ids!(reader_app)).changed(actions).is_some()
            || self.view.drop_down(cx, ids!(sharing_lifetime)).changed(actions).is_some()
        {
            self.update_recipient_form(cx);
        }
        if self.view.drop_down(cx, ids!(context_choice)).changed(actions).is_some()
            || self.view.drop_down(cx, ids!(decision_choice)).changed(actions).is_some()
            || self.view.drop_down(cx, ids!(action_choice)).changed(actions).is_some()
            || self.view.drop_down(cx, ids!(action_session)).changed(actions).is_some()
        {
            self.update_diagnostic_details(cx);
            self.update_recipient_form(cx);
        }
        if self.view.button(cx, ids!(review_button)).clicked(actions) {
            if let Err(error) = self.review_decision(cx) { self.show_error(error); }
        }
        if self.view.button(cx, ids!(add_button)).clicked(actions) {
            let result = self.sharing_action(cx);
            self.submit(cx, result);
        }
        if self.view.button(cx, ids!(remove_button)).clicked(actions) {
            let index = self.view.drop_down(cx, ids!(remove_choice)).selected_item();
            let result = self.grants.get(index).map(|grant| A2AppOp::RevokeFlowSharing(grant.id))
                .ok_or_else(|| "Select a sharing rule to remove.".to_string());
            self.submit(cx, result);
        }
        if self.view.button(cx, ids!(authority_button)).clicked(actions) {
            let result = self.authority_action(cx);
            self.submit(cx, result);
        }
        if self.view.button(cx, ids!(authority_remove_button)).clicked(actions) {
            let index = self.view.drop_down(cx, ids!(authority_remove_choice)).selected_item();
            let result = self.authorities.get(index).map(|grant| A2AppOp::RevokeFlowAuthority(grant.id))
                .ok_or_else(|| "Select an action permission to remove.".to_string());
            self.submit(cx, result);
        }
        self.view.redraw(cx);
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.refresh_diagnostics(cx);
        self.refresh_rules(cx);
        self.refresh_activity(cx);
        self.view.draw_walk(cx, scope, walk)
    }
}

impl DataSharing {
    fn configure(&mut self, cx: &mut Cx) {
        self.activity_key = None;
        self.account = super::information_flow::account().unwrap_or_default();
        self.rooms = if cx.has_global::<RoomsListRef>() {
            cx.get_global::<RoomsListRef>().permission_targets().into_iter()
                .filter(|(_, _, space)| !space).map(|(id, name, _)| (id, name)).collect()
        } else { Vec::new() };
        self.apps = with_a2app(|state| state.registry.iter().map(|app| (app.id.clone(), app.name.clone())).collect()).unwrap_or_default();
        self.view.drop_down(cx, ids!(reader_app)).set_labels(cx, self.apps.iter().map(|(id, name)| format!("{name} ({id})")).collect());
        self.view.drop_down(cx, ids!(reader_app)).set_selected_item(cx, 0);
        self.view.drop_down(cx, ids!(lifetime_room)).set_labels(cx, self.rooms.iter().map(|(id, name)| format!("{name} ({id})")).collect());
        self.view.drop_down(cx, ids!(lifetime_room)).set_selected_item(cx, 0);
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
        self.view.drop_down(cx, ids!(source_choice)).set_labels(cx, self.sources.iter().map(|(_, label)| label.clone()).collect());
        self.view.drop_down(cx, ids!(source_choice)).set_selected_item(cx, 0);
        self.view.drop_down(cx, ids!(target_room)).set_labels(cx, self.rooms.iter().map(|(id, name)| format!("{name} ({id})")).collect());
        self.view.drop_down(cx, ids!(target_room)).set_selected_item(cx, 0);
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
        self.view.redraw(cx);
    }

    fn selected_source(&self, cx: &Cx) -> Result<Source, String> {
        self.sources.get(self.view.drop_down(cx, ids!(source_choice)).selected_item())
            .map(|(source, _)| source.clone()).ok_or_else(|| "Sign in and select a data source.".into())
    }

    fn selected_recipient(&self, cx: &Cx) -> Result<Recipient, String> {
        match self.view.drop_down(cx, ids!(recipient_kind)).selected_item() {
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
        let kind = self.view.drop_down(cx, ids!(recipient_kind)).selected_item();
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
        let reader_kind = self.view.drop_down(cx, ids!(reader_kind)).selected_item();
        self.view.widget(cx, ids!(reader_app_section)).set_visible(cx, reader_kind == 1);
        let room_lifetime = self.view.drop_down(cx, ids!(sharing_lifetime)).selected_item() == 1;
        self.view.widget(cx, ids!(lifetime_room_section)).set_visible(cx, room_lifetime);
        let preview = self.selected_reader(cx).map(|reader| self.reader_label(&reader))
            .unwrap_or_else(|error| error);
        self.view.label(cx, ids!(reader_preview)).set_text(cx, &preview);
        self.view.redraw(cx);
    }

    fn refresh_rules(&mut self, cx: &mut Cx) {
        let source = self.selected_source(cx);
        let result = source.as_ref().map_err(Clone::clone).and_then(|source| {
            Ok(flow::sharing_grants()?.into_iter().filter(|grant| &grant.source == source).collect::<Vec<_>>())
        });
        match result {
            Ok(grants) => {
                self.view.widget(cx, ids!(sharing_error)).set_visible(cx, false);
                let labels = grants.iter().map(|grant| format!("{} · {} · {}",
                    self.recipient_label(&grant.recipient), self.reader_label(&grant.reader), self.duration_label(&grant.duration))).collect::<Vec<_>>();
                let summary = if labels.is_empty() { "No additional recipients. Data sharing is protected by default.".into() }
                    else { bullets(&labels) };
                self.view.label(cx, ids!(current_rules)).set_text(cx, &summary);
                self.view.widget(cx, ids!(remove_section)).set_visible(cx, !labels.is_empty());
                if self.grants != grants {
                    self.grants = grants;
                    self.view.drop_down(cx, ids!(remove_choice)).set_labels(cx, labels);
                    self.view.drop_down(cx, ids!(remove_choice)).set_selected_item(cx, 0);
                }
            }
            Err(error) => {
                self.view.label(cx, ids!(sharing_error)).set_text(cx, &error);
                self.view.widget(cx, ids!(sharing_error)).set_visible(cx, true);
                self.view.label(cx, ids!(current_rules)).set_text(cx, "Sharing rules are unavailable; private data remains protected.");
                self.view.widget(cx, ids!(remove_section)).set_visible(cx, false);
                self.grants.clear();
            }
        }
        let hint = match source {
            Ok(Source::Room { .. }) => "Default: data may return to its original room in this account. Other rooms and services need a sharing rule.",
            Ok(Source::UnknownPrivate) => "Unknown sources cannot be released. There is no rule or reset that makes existing private data public.",
            _ => "Default: account data stays in Robrix. Sending it to a model service, website or room requires a rule below.",
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor() -> (Cx, WidgetRef) {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let widget = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            makepad_code_editor::script_mod(vm);
            crate::shared::script_mod(vm);
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
        editor.view.drop_down(&cx, ids!(recipient_kind)).set_selected_item(&mut cx, 1);
        editor.view.text_input(&cx, ids!(network_url)).set_text(&mut cx, "https://EXAMPLE.com:443/path?query=1");
        assert_eq!(editor.selected_recipient(&cx).unwrap(), Recipient::NetworkOrigin("https://example.com".into()));
        editor.view.drop_down(&cx, ids!(recipient_kind)).set_selected_item(&mut cx, 2);
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
        editor.view.drop_down(&cx, ids!(recipient_kind)).set_selected_item(&mut cx, 1);
        assert!(editor.selected_recipient(&cx).is_err());
        editor.view.text_input(&cx, ids!(network_url)).set_text(&mut cx, "https://user:secret@example.com");
        assert!(editor.selected_recipient(&cx).is_err());
        editor.view.drop_down(&cx, ids!(recipient_kind)).set_selected_item(&mut cx, 2);
        assert!(editor.selected_recipient(&cx).is_err());
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
        editor.view.drop_down(&cx, ids!(recipient_kind)).set_selected_item(&mut cx, 1);
        editor.view.text_input(&cx, ids!(network_url)).set_text(&mut cx, "https://example.org/private/path");
        match editor.sharing_action(&cx).unwrap() {
            A2AppOp::GrantFlowSharing { source: actual, reader, duration, recipient } => {
                assert_eq!(actual, source);
                assert_eq!(reader, ReaderScope::Context(snapshot.context));
                assert_eq!(duration, SharingDuration::RobrixSession);
                assert_eq!(recipient, Recipient::NetworkOrigin("https://example.org".into()));
            }
            _ => panic!("wrong action"),
        }
        editor.view.drop_down(&cx, ids!(reader_kind)).set_selected_item(&mut cx, 1);
        editor.view.drop_down(&cx, ids!(sharing_lifetime)).set_selected_item(&mut cx, 1);
        match editor.sharing_action(&cx).unwrap() {
            A2AppOp::GrantFlowSharing { reader, duration, .. } => {
                assert_eq!(reader, ReaderScope::App { account: editor.account.clone(), app: "weather".into() });
                assert_eq!(duration, SharingDuration::RoomSession { account: editor.account.clone(), room: "!expiry:example.org".into() });
            }
            _ => panic!("wrong action"),
        }
        editor.sources[0].0 = Source::UnknownPrivate;
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
        editor.view.drop_down(&cx, ids!(action_choice)).set_labels(&mut cx, vec!["Select action".into(), "Fixture action".into()]);
        editor.view.drop_down(&cx, ids!(action_choice)).set_selected_item(&mut cx, 1);
        editor.view.drop_down(&cx, ids!(action_session)).set_selected_item(&mut cx, 1);
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
        editor.view.drop_down(&cx, ids!(decision_choice)).set_labels(&mut cx, vec!["Select decision".into(), "Fixture decision".into()]);
        editor.view.drop_down(&cx, ids!(decision_choice)).set_selected_item(&mut cx, 1);
        editor.update_diagnostic_details(&mut cx);
        editor.review_decision(&mut cx).unwrap();
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
        editor.view.drop_down(&cx, ids!(action_choice)).set_labels(&mut cx, vec!["Select action".into(), "Fixture action".into()]);
        editor.view.drop_down(&cx, ids!(action_choice)).set_selected_item(&mut cx, 1);
        editor.update_diagnostic_details(&mut cx);
        assert!(editor.view.label(&cx, ids!(action_details)).text().contains("The exact reviewed message"));
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
        assert!(editor.authority_action(&cx).is_err());
        editor.snapshots[0].epoch += 1;
        assert!(editor.authority_action(&cx).is_err());
    }

    #[test]
    fn sharing_denials_name_the_blocking_control_and_how_to_change_it() {
        let (_cx, widget) = editor();
        let mut editor = widget.borrow_mut::<DataSharing>().unwrap();
        editor.account = "@alice:example.org".into();
        let snapshot = context_fixture();
        let recipient = Recipient::NetworkOrigin("https://example.org".into());
        let text = editor.sharing_remedy(&snapshot.label, &recipient);
        assert!(text.contains("Private data sharing"));
        assert!(text.contains("Who may share this source"));
        assert!(text.contains("Allow sharing with this recipient"));
        assert!(editor.sharing_remedy(&[Source::UnknownPrivate].into(), &recipient).contains("No sharing toggle"));
    }

}
