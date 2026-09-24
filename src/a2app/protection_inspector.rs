//! Explain the same room and capability decisions used by the host.

use std::collections::BTreeMap;
use makepad_widgets::*;
use a2app_core::capabilities::{self, Capability, Scope as CapabilityScope};
use a2app_core::information_flow::{self as flow, ContextId, ContextSnapshot, Label, Source};
use a2app_core::manifest::{A2AppScope, MiniAppManifest};
use a2app_core::permissions::{
    agent_subject, CapabilityDecisionReason, CapabilityEvaluation, Effective, Permission,
    PermissionContext, PermissionStore, PolicyDecision, RoomAccess, RoomPolicyEvaluation,
    RoomPolicyReason, RoomScope,
};
use crate::home::rooms_list::RoomsListRef;
use super::mini_apps_screen::AccessRuleKey;
use super::permission_choices::*;
use super::runtime::with_a2app;

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    mod.widgets.ProtectionControlButton = RobrixNeutralIconButton {
        width: Fill, height: Fit, padding: 10
        label_walk: Walk{width: Fill, height: Fit}
        icon_walk: Walk{width: 0, height: 0, margin: 0}
    }
    mod.widgets.ProtectionInspector = set_type_default() do #(ProtectionInspector::register_widget(vm)) {
        width: Fill, height: Fill, flow: Down
        content := ScrollYView {
            width: Fill, height: Fill, flow: Down, spacing: 10, padding: 15
            page_choice := PermissionChoices {
                labels: ["Room access", "Check a mini-app"]
                horizontal: true, tabs: true
            }
            target_picker := View {
                width: Fill, height: Fit, flow: Down, spacing: 10
                SubsectionLabel { text: "Which room or space?", margin: Inset{top: 8} }
                PermissionOptionLabel { text: "See what mini-apps and agents can read or change, and which settings control it." }
                target := PermissionDropDown {}
                keep_target := RobrixNeutralIconButton {
                    visible: false, padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}
                    text: "Keep current room or space"
                }
            }
            target_summary := View {
                visible: false, width: Fill, height: Fit, flow: Down, spacing: 10
                target_title := SubsectionLabel { margin: Inset{top: 8} }
                change_target := RobrixNeutralIconButton {
                    padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}
                    text: "Change room or space"
                }
            }
            room_page := View {
                visible: false, width: Fill, height: Fit, flow: Down, spacing: 10
                SubsectionLabel { text: "Read access", margin: Inset{top: 8} }
                read_details := PermissionOptionLabel {}
                read_controls_section := View { width: Fill, height: Fit, flow: Down, spacing: 6 }
                LineH { margin: Inset{top: 8, bottom: 8} }
                SubsectionLabel { text: "Write access", margin: 0 }
                write_details := PermissionOptionLabel {}
                write_controls_section := View { width: Fill, height: Fit, flow: Down, spacing: 6 }
                PermissionOptionLabel {
                    text: "These rules apply to every mini-app and agent. Their own permissions and data-sharing checks still apply."
                }
                target_details := PermissionOptionLabel {}
                retained_toggle := RobrixNeutralIconButton {
                    padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}
                    text: "Show previously accessed data"
                }
                retained_page := View {
                    visible: false, width: Fill, height: Fit, flow: Down, spacing: 10
                    SubsectionLabel { text: "Previously accessed data", margin: 0 }
                    PermissionOptionLabel {
                        text: "Blocking new reads does not erase data already read or remove sharing rules. These records show possible use in saved app data, agent history and app code, even after they stop running."
                    }
                    retained_details := PermissionOptionLabel {}
                    PermissionOptionLabel {
                        text: "These records do not prove that the original messages are still stored. To prevent future sharing, remove the relevant data-sharing rules."
                    }
                    sharing := RobrixNeutralIconButton {
                        padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}
                        text: "Manage data sharing"
                    }
                }
            }
            subject_page := View {
                visible: false, width: Fill, height: Fit, flow: Down, spacing: 10
                subject_picker := View {
                    width: Fill, height: Fit, flow: Down, spacing: 10
                    SubsectionLabel { text: "Which mini-app or agent?", margin: Inset{top: 8} }
                    PermissionOptionLabel { text: "Check a specific action and find the setting that allows or blocks it." }
                    subject := PermissionDropDown {}
                    keep_subject := RobrixNeutralIconButton {
                        visible: false, padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}
                        text: "Keep current mini-app or agent"
                    }
                }
                subject_check := View {
                    visible: false, width: Fill, height: Fit, flow: Down, spacing: 10
                    subject_title := SubsectionLabel { margin: Inset{top: 8} }
                    change_subject := RobrixNeutralIconButton {
                        padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}
                        text: "Change mini-app or agent"
                    }
                    SubsectionLabel { text: "What does it want to do?", margin: Inset{top: 8} }
                    capability := PermissionDropDown {}
                    capability_details := PermissionOptionLabel {}
                    capability_rule := RobrixNeutralIconButton {
                        padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}
                        text: "Change this permission"
                    }
                    context_toggle := RobrixNeutralIconButton {
                        padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}
                        text: "About this app check"
                    }
                    subject_context := View {
                        visible: false, width: Fill, height: Fit, flow: Down
                        subject_details := PermissionOptionLabel {}
                    }
                }
            }
            check_footer := View {
                visible: false, width: Fill, height: Fit, flow: Down, spacing: 10
                LineH { margin: Inset{top: 8, bottom: 8} }
                refresh := RobrixNeutralIconButton {
                    padding: 10, icon_walk: Walk{width: 0, height: 0, margin: 0}
                    text: "Refresh access check"
                }
                snapshot_status := PermissionOptionLabel {}
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum ProtectionInspectorAction {
    Global,
    Rule(AccessRuleKey),
    Ability { subject: String, permission: Permission, capability: Option<String>, scope: RoomScope },
    SubjectInfo(String),
    Sharing,
}

#[derive(Clone, Debug, PartialEq)]
struct SubjectChoice {
    subject: String,
    label: String,
    context: Option<ContextId>,
    origin_room: Option<String>,
    active: bool,
}

struct Explanation {
    text: String,
    control: Option<ProtectionInspectorAction>,
}

#[derive(Script, ScriptHook, Widget)]
pub struct ProtectionInspector {
    #[deref] view: View,
    #[rust] account: String,
    #[rust] targets: Vec<(String, String, bool)>,
    #[rust] subjects: Vec<SubjectChoice>,
    #[rust] snapshots: Vec<ContextSnapshot>,
    #[rust] provenance_error: Option<String>,
    #[rust] code_sources: Vec<(String, String, Result<Label, String>)>,
    #[rust] read_controls: Vec<ProtectionInspectorAction>,
    #[rust] write_controls: Vec<ProtectionInspectorAction>,
    #[rust] capability_control: Option<ProtectionInspectorAction>,
    #[rust] configured: bool,
    #[rust] selected_target_id: Option<String>,
    #[rust] selected_subject_choice: Option<SubjectChoice>,
    #[rust] choosing_target: bool,
    #[rust] choosing_subject: bool,
    #[rust] showing_retained: bool,
    #[rust] showing_context: bool,
}

impl Widget for ProtectionInspector {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if self.refresh_account(cx) { return; }
        self.view.handle_event(cx, event, scope);
        let Event::Actions(actions) = event else { return };
        if self.view.permission_choices(cx, ids!(page_choice)).changed(actions).is_some() {
            self.reset_scroll(cx);
            self.update_page(cx);
            return;
        }
        if self.view.button(cx, ids!(change_target)).clicked(actions) {
            self.choosing_target = true;
            self.showing_retained = false;
            self.reset_scroll(cx);
            self.update_page(cx);
            return;
        }
        if self.view.button(cx, ids!(change_subject)).clicked(actions) {
            self.choosing_subject = true;
            self.showing_context = false;
            self.reset_scroll(cx);
            self.update_page(cx);
            return;
        }
        if self.view.button(cx, ids!(refresh)).clicked(actions) {
            self.configure(cx);
            return;
        } else if let Some(index) = self.view.drop_down(cx, ids!(target)).changed(actions) {
            self.selected_target_id = index.checked_sub(1).and_then(|index| self.targets.get(index)).map(|target| target.0.clone());
            self.choosing_target = false;
            self.showing_retained = false;
            self.update_details(cx);
            self.reset_scroll(cx);
            return;
        } else if let Some(index) = self.view.drop_down(cx, ids!(subject)).changed(actions) {
            self.selected_subject_choice = index.checked_sub(1).and_then(|index| self.subjects.get(index)).cloned();
            self.choosing_subject = false;
            self.showing_context = false;
            self.update_details(cx);
            self.reset_scroll(cx);
            return;
        } else if self.view.drop_down(cx, ids!(capability)).changed(actions).is_some() {
            self.update_details(cx);
            return;
        }
        if self.view.button(cx, ids!(retained_toggle)).clicked(actions) {
            self.showing_retained = !self.showing_retained;
            self.update_page(cx);
            return;
        }
        if self.view.button(cx, ids!(context_toggle)).clicked(actions) {
            self.showing_context = !self.showing_context;
            self.update_page(cx);
            return;
        }
        if self.view.button(cx, ids!(keep_target)).clicked(actions) {
            self.choosing_target = false;
            self.update_page(cx);
            return;
        }
        if self.view.button(cx, ids!(keep_subject)).clicked(actions) {
            self.choosing_subject = false;
            self.update_page(cx);
            return;
        }
        if self.choosing_target || self.selected_target(cx).is_none() { return; }
        let page = self.view.permission_choices(cx, ids!(page_choice)).selected_item();
        if page == 0 {
            for (section, controls) in [(ids!(read_controls_section), &self.read_controls), (ids!(write_controls_section), &self.write_controls)] {
                if let Some(buttons) = self.view.view(cx, section).borrow() {
                    for ((_, button), control) in buttons.children.iter().zip(controls) {
                        if button.as_button().clicked(actions) { cx.action(control.clone()); }
                    }
                }
            }
            if self.showing_retained && self.view.button(cx, ids!(sharing)).clicked(actions) {
                cx.action(ProtectionInspectorAction::Sharing);
            }
        } else if page == 1 && !self.choosing_subject && self.selected_subject(cx).is_some()
            && self.view.button(cx, ids!(capability_rule)).clicked(actions)
            && let Some(control) = &self.capability_control
        { cx.action(control.clone()); }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.refresh_account(cx);
        self.view.draw_walk(cx, scope, walk)
    }
}

impl ProtectionInspector {
    fn refresh_account(&mut self, cx: &mut Cx) -> bool {
        if self.account == super::information_flow::account().unwrap_or_default() { return false; }
        self.configure(cx);
        true
    }

    fn reset_scroll(&self, cx: &mut Cx) {
        self.view.view(cx, ids!(content)).set_scroll_pos(cx, Vec2d::default());
    }

    fn update_page(&mut self, cx: &mut Cx) {
        let page = self.view.permission_choices(cx, ids!(page_choice)).selected_item();
        let has_target = self.selected_target(cx).is_some() && !self.choosing_target;
        let has_subject = self.selected_subject(cx).is_some() && !self.choosing_subject;
        self.view.widget(cx, ids!(target_picker)).set_visible(cx, !has_target);
        self.view.widget(cx, ids!(keep_target)).set_visible(cx, self.selected_target(cx).is_some());
        self.view.widget(cx, ids!(keep_subject)).set_visible(cx, self.selected_subject(cx).is_some());
        self.view.widget(cx, ids!(target_summary)).set_visible(cx, has_target);
        self.view.widget(cx, ids!(room_page)).set_visible(cx, has_target && page == 0);
        self.view.widget(cx, ids!(subject_page)).set_visible(cx, has_target && page == 1);
        self.view.widget(cx, ids!(subject_picker)).set_visible(cx, !has_subject);
        self.view.widget(cx, ids!(subject_check)).set_visible(cx, has_subject);
        self.view.widget(cx, ids!(retained_page)).set_visible(cx, self.showing_retained);
        self.view.widget(cx, ids!(subject_context)).set_visible(cx, self.showing_context);
        self.view.widget(cx, ids!(check_footer)).set_visible(cx, has_target && (page == 0 || has_subject));
        self.view.button(cx, ids!(retained_toggle)).set_text(cx,
            if self.showing_retained { "Hide previously accessed data" } else { "Show previously accessed data" });
        self.view.button(cx, ids!(context_toggle)).set_text(cx,
            if self.showing_context { "Hide app details" } else { "About this app check" });
        self.view.redraw(cx);
    }

    fn configure(&mut self, cx: &mut Cx) {
        let account = super::information_flow::account().unwrap_or_default();
        let account_changed = self.account != account;
        let previous_target = self.selected_target(cx).filter(|_| !account_changed).map(|target| target.0.clone());
        let previous_subject = self.selected_subject(cx).filter(|_| !account_changed).cloned();
        self.account = account;
        if account_changed {
            self.configured = false;
            self.view.permission_choices(cx, ids!(page_choice)).set_selected_item(cx, 0);
            self.selected_target_id = None;
            self.selected_subject_choice = None;
            self.choosing_target = false;
            self.choosing_subject = false;
            self.showing_retained = false;
            self.showing_context = false;
            self.reset_scroll(cx);
        }
        match flow::retained_contexts() {
            Ok(snapshots) => {
                self.snapshots = snapshots.into_iter().filter(|snapshot| snapshot.context.account() == self.account).collect();
                self.provenance_error = None;
            }
            Err(error) => { self.snapshots.clear(); self.provenance_error = Some(error); }
        }
        let mut targets = BTreeMap::new();
        if cx.has_global::<RoomsListRef>() {
            for (id, name, space) in cx.get_global::<RoomsListRef>().permission_targets() {
                targets.insert(id, (name, space));
            }
        }
        let apps = with_a2app(|state| {
            for room in state.permissions.room_rules().keys() {
                targets.entry(room.clone()).or_insert_with(|| (room.clone(), false));
            }
            for space in state.permissions.space_rules().keys() {
                targets.entry(space.clone()).or_insert_with(|| (space.clone(), true));
            }
            state.registry.iter().map(|app| (app.id.clone(), app.name.clone(), app.scope.clone())).collect::<Vec<_>>()
        }).unwrap_or_default();
        self.code_sources = apps.iter().map(|(app, name, _)| (app.clone(), name.clone(), flow::code_labels(app))).collect();
        for (_, _, source) in &self.code_sources {
            if let Ok(sources) = source {
                for source in sources {
                    if let Source::Room { account, room } = source && account == &self.account {
                        targets.entry(room.clone()).or_insert_with(|| (room.clone(), false));
                    }
                }
            }
        }
        for snapshot in &self.snapshots {
            if let Some(room) = snapshot.context.room() {
                targets.entry(room.into()).or_insert_with(|| (room.into(), false));
            }
            for source in &snapshot.label {
                if let Source::Room { account, room } = source && account == &self.account {
                    targets.entry(room.clone()).or_insert_with(|| (room.clone(), false));
                }
            }
        }
        self.targets = targets.into_iter().map(|(id, (name, space))| (id, name, space)).collect();
        self.targets.sort_by_cached_key(|target| (target.1.to_lowercase(), target.0.clone()));
        self.selected_target_id = previous_target.filter(|id| self.targets.iter().any(|target| &target.0 == id));
        let mut target_labels = vec!["Choose a room or space…".into()];
        target_labels.extend(self.targets.iter().map(|(_, name, space)|
            format!("{}: {name}", if *space { "Space" } else { "Room" })));
        self.view.drop_down(cx, ids!(target)).set_labels(cx, target_labels);
        self.view.drop_down(cx, ids!(target)).set_selected_item(cx,
            self.targets.iter().position(|target| Some(&target.0) == self.selected_target_id.as_ref()).map(|index| index + 1).unwrap_or(0));

        self.subjects = self.snapshots.iter().map(|snapshot| {
            let context = &snapshot.context;
            let subject = context.app().map(str::to_owned).unwrap_or_else(|| agent_subject(context.room().unwrap_or_default()));
            SubjectChoice {
                subject, label: format!("{} · {}", self.context_label(context, &apps), if snapshot.epoch == 0 { "Stopped; data protection retained" } else { "Active" }),
                context: Some(context.clone()), origin_room: context.room().map(str::to_owned), active: snapshot.epoch != 0,
            }
        }).collect();
        for (app, name, scope) in &apps {
            if !self.subjects.iter().any(|subject| subject.subject == *app) {
                self.subjects.push(SubjectChoice {
                    subject: app.clone(), label: format!("{name} ({app}) · No saved activity"), context: None, active: false,
                    origin_room: match scope { A2AppScope::Room { room_id } => Some(room_id.clone()), _ => None },
                });
            }
        }
        let mut subject_labels = vec!["Choose a mini-app or agent…".into()];
        subject_labels.extend(self.subjects.iter().map(|subject| {
            let app_name = |app: &str| apps.iter().find(|candidate| candidate.0 == app)
                .map(|candidate| candidate.1.clone()).unwrap_or_else(|| app.into());
            let room_name = |room: &str| self.targets.iter().find(|target| target.0 == room)
                .map(|target| target.1.clone()).unwrap_or_else(|| room.into());
            match &subject.context {
                Some(ContextId::App { app, room, .. }) => format!("{} · {}", app_name(app), room.as_deref().map(room_name).unwrap_or_else(|| "Account-wide".into())),
                Some(ContextId::PublicApp { app, .. }) => format!("{} · Public mode", app_name(app)),
                Some(ContextId::Agent { room, .. }) => format!("Agent · {}", room_name(room)),
                None => app_name(&subject.subject),
            }
        }));
        self.view.drop_down(cx, ids!(subject)).set_labels(cx, subject_labels);
        self.selected_subject_choice = previous_subject.and_then(|previous| self.subjects.iter().find(|subject|
            subject.subject == previous.subject && subject.context == previous.context).cloned());
        self.view.drop_down(cx, ids!(subject)).set_selected_item(cx,
            self.subjects.iter().position(|subject| self.selected_subject_choice.as_ref() == Some(subject)).map(|index| index + 1).unwrap_or(0));
        let previous_capability = if self.configured { self.view.drop_down(cx, ids!(capability)).selected_item() }
            else { capabilities::CATALOG.iter().position(|cap| cap.id == "matrix.rooms.messages.read").unwrap_or(0) };
        self.view.drop_down(cx, ids!(capability)).set_labels(cx, capabilities::CATALOG.iter().map(|cap| cap.title.to_string()).collect());
        self.view.drop_down(cx, ids!(capability)).set_selected_item(cx, previous_capability);
        self.configured = true;
        self.view.label(cx, ids!(snapshot_status)).set_text(cx, "Checked just now. Refresh after changing settings or running an app.");
        self.update_details(cx);
    }

    fn selected_target(&self, _cx: &Cx) -> Option<&(String, String, bool)> {
        self.targets.iter().find(|target| Some(&target.0) == self.selected_target_id.as_ref())
    }

    fn selected_subject(&self, _cx: &Cx) -> Option<&SubjectChoice> {
        let selected = self.selected_subject_choice.as_ref()?;
        self.subjects.iter().find(|subject| subject.subject == selected.subject && subject.context == selected.context)
    }

    fn room_label(&self, room: &str) -> String {
        self.targets.iter().find(|target| target.0 == room).map(|target| format!("{} ({room})", target.1)).unwrap_or_else(|| room.into())
    }

    fn context_label(&self, context: &ContextId, apps: &[(String, String, A2AppScope)]) -> String {
        let app_name = |app: &str| apps.iter().find(|candidate| candidate.0 == app)
            .map(|candidate| format!("{} ({app})", candidate.1)).unwrap_or_else(|| app.into());
        match context {
            ContextId::App { app, room, .. } => format!("{} · {}", app_name(app), room.as_deref().map(|room| self.room_label(room)).unwrap_or_else(|| "Account-wide".into())),
            ContextId::PublicApp { app, .. } => format!("{} · Public mode", app_name(app)),
            ContextId::Agent { room, .. } => format!("Agent · {}", self.room_label(room)),
        }
    }

    fn update_details(&mut self, cx: &mut Cx) {
        self.read_controls.clear();
        self.write_controls.clear();
        self.capability_control = None;
        let target = self.selected_target(cx).cloned();
        let subject = self.selected_subject(cx).cloned();
        let cap = capabilities::CATALOG.get(self.view.drop_down(cx, ids!(capability)).selected_item());
        let details = target.as_ref().and_then(|(room, _, space)| with_a2app(|state| {
            let read = explain_room_rules(&state.permissions, RoomAccess::Read, room, *space, |id| self.room_label(id));
            let write = explain_room_rules(&state.permissions, RoomAccess::Write, room, *space, |id| self.room_label(id));
            let capability = subject.as_ref().zip(cap).map(|(subject, cap)| {
                explain_capability(&state.permissions, state.registry.get(&subject.subject), subject, cap, room, *space, |id| self.room_label(id))
            });
            (read, write, capability)
        }));
        if let Some((read, write, capability)) = details {
            self.view.label(cx, ids!(read_details)).set_text(cx, &read.0);
            self.view.label(cx, ids!(write_details)).set_text(cx, &write.0);
            self.read_controls = read.1;
            self.write_controls = write.1;
            self.view.label(cx, ids!(capability_details)).set_text(cx, capability.as_ref().map(|result| result.text.as_str()).unwrap_or("Choose a mini-app or agent and the action you want to check."));
            self.capability_control = capability.and_then(|result| result.control);
        } else {
            for id in [ids!(read_details), ids!(write_details), ids!(capability_details)] {
                self.view.label(cx, id).set_text(cx, "Sign in and choose a room or space to check its access.");
            }
        }
        let target_hint = if target.as_ref().is_some_and(|target| target.2) {
            "Checking this space itself. Space rules also cover its rooms and nested spaces. Choose a room to check all the rules that apply to that room."
        } else { "Checking this room, including rules inherited from its spaces. A Block rule always wins over an Allow rule." };
        self.view.label(cx, ids!(target_title)).set_text(cx, &target.as_ref().map(|(_, name, space)|
            format!("{}: {name}", if *space { "Space" } else { "Room" })).unwrap_or_default());
        self.view.label(cx, ids!(target_details)).set_text(cx, target_hint);
        self.view.label(cx, ids!(subject_title)).set_text(cx,
            &self.view.drop_down(cx, ids!(subject)).borrow().map(|choice| choice.selected_item_label()).unwrap_or_default());
        let context_text = subject.as_ref().map(|subject| {
            let activity = if subject.active { "Running when last checked." } else { "Not running when last checked. Starting it again requires fresh session and data-sharing checks." };
            let clearance = if matches!(subject.context, Some(ContextId::PublicApp { .. })) {
                "Public mode: this app cannot receive private room or account data, even when a permission is allowed."
            } else { "Permission to perform this action does not also approve data sharing or instructions found in outside content." };
            format!("{}\n{activity}\n{clearance}", subject.label)
        }).unwrap_or_else(|| "No mini-app or agent is available. Install a mini-app or start an agent to check its permissions.".into());
        self.view.label(cx, ids!(subject_details)).set_text(cx, &context_text);
        let retained = match (&self.provenance_error, &target) {
            (Some(error), _) => format!("Previously accessed data could not be checked: {error}. Robrix cannot tell which apps or agents may already know this room’s data."),
            (_, Some((room, _, _))) => format!("App data and agent history:\n{}\n\nMini-app code:\n{}",
                retained_text(&self.snapshots, &self.account, room, |context|
                    self.subjects.iter().find(|subject| subject.context.as_ref() == Some(context)).map(|subject| subject.label.clone()).unwrap_or_else(|| format!("{context:?}"))),
                retained_code_text(&self.code_sources, &self.account, room)),
            _ => "Choose a room or space to check previously accessed data.".into(),
        };
        self.view.label(cx, ids!(retained_details)).set_text(cx, &retained);
        self.populate_controls(cx);
        self.view.widget(cx, ids!(capability_rule)).set_visible(cx, self.capability_control.is_some());
        self.view.button(cx, ids!(capability_rule)).set_text(cx, match &self.capability_control {
            Some(ProtectionInspectorAction::Global) => "Change room access",
            Some(ProtectionInspectorAction::Rule(AccessRuleKey::Room(_))) => "Change room rule",
            Some(ProtectionInspectorAction::Rule(AccessRuleKey::Space(_))) => "Change space rule",
            Some(ProtectionInspectorAction::SubjectInfo(_)) => "Open app or agent settings",
            _ => "Change this permission",
        });
        self.update_page(cx);
    }

    fn populate_controls(&self, cx: &mut Cx) {
        for (section, controls) in [(ids!(read_controls_section), &self.read_controls), (ids!(write_controls_section), &self.write_controls)] {
            let section_view = self.view.view(cx, section);
            let Some(mut container) = section_view.borrow_mut() else { continue };
            // Rebuild controls on each check so stale button actions cannot open a different setting.
            container.children.clear();
            for (index, control) in controls.iter().enumerate() {
                let text = match control {
                    ProtectionInspectorAction::Global => "Change default room access".into(),
                    ProtectionInspectorAction::Rule(AccessRuleKey::Room(room)) => format!("Change room rule: {}", self.targets.iter().find(|target| &target.0 == room).map(|target| target.1.as_str()).unwrap_or(room)),
                    ProtectionInspectorAction::Rule(AccessRuleKey::Space(space)) => format!("Change space rule: {}", self.targets.iter().find(|target| &target.0 == space).map(|target| target.1.as_str()).unwrap_or(space)),
                    _ => "Change permission".into(),
                };
                let button = cx.with_vm(|vm| {
                    let value = script_eval!(vm, { mod.widgets.ProtectionControlButton {} });
                    WidgetRef::script_from_value(vm, value)
                });
                button.set_text(cx, &text);
                container.children.push((LiveId(index as u64 + 1), button));
            }
        }
    }
}

fn explain_room_rules(store: &PermissionStore, access: RoomAccess, room: &str, space: bool, label: impl Fn(&str) -> String)
    -> (String, Vec<ProtectionInspectorAction>)
{
    let evaluation = store.room_policy_evaluation(Some(room), access);
    let blockers = store.room_policy_blockers(Some(room), access);
    if blockers.is_empty() {
        let explanation = explain_room(evaluation, access, room, space, label);
        return (explanation.text, explanation.control.into_iter().collect());
    }
    let mut lines = vec!["Blocked by room access settings. Every setting listed below must allow access before it can proceed:".into()];
    let mut controls = Vec::new();
    for reason in blockers {
        let explanation = explain_room(RoomPolicyEvaluation { decision: PolicyDecision::Deny, reason }, access, room, space, &label);
        let detail = explanation.text.split_once('\n').map(|(_, reason)| reason).unwrap_or(&explanation.text);
        lines.push(format!("• {detail}"));
        controls.extend(explanation.control);
    }
    (lines.join("\n"), controls)
}

fn explain_room(evaluation: RoomPolicyEvaluation<'_>, access: RoomAccess, room: &str, space: bool, label: impl Fn(&str) -> String) -> Explanation {
    use RoomPolicyReason::*;
    let verb = if access == RoomAccess::Read { "read" } else { "write" };
    let status = match evaluation.decision {
        PolicyDecision::Allow => "Allowed by room access settings",
        PolicyDecision::Ask => "No room-wide allowance; the app or agent must have permission",
        PolicyDecision::Deny => "Blocked by room access settings",
    };
    let (reason, control) = match evaluation.reason {
        WriteMasterOff => ("Allow room writes is off. Turn it on in Mini Apps → Room and space access → Allow room writes to restore your saved write rules.".into(), ProtectionInspectorAction::Global),
        GlobalBlock => (format!("The default {verb} setting blocks every room. Open Room and space access → Change {verb} access to change it. Allowing an individual room cannot override this block."), ProtectionInspectorAction::Global),
        GlobalDefault => (format!("The default {verb} setting decides this result. Change it under Room and space access → Change {verb} access, or add a room or space rule."), ProtectionInspectorAction::Global),
        RoomRule { room } => (format!("The {verb} rule for {} decides this result. Edit that room rule; any matching block still wins.", label(room)), ProtectionInspectorAction::Rule(AccessRuleKey::Room(room.into()))),
        SpaceRule { space, ancestor } => (format!("The {verb} rule for {}{} decides this result. Edit that space rule; any matching block still wins.", if ancestor { "ancestor space " } else { "space " }, label(space)), ProtectionInspectorAction::Rule(AccessRuleKey::Space(space.into()))),
        UnresolvedSpaceHierarchy { space, rule } => (format!("Robrix has not resolved whether this target belongs to space {}. Its {} {verb} rule cannot yet be safely evaluated. Wait for room/space synchronization and refresh; inspect that space rule if needed.", label(space), if rule == PolicyDecision::Deny { "Block" } else { "Allow" }), ProtectionInspectorAction::Rule(AccessRuleKey::Space(space.into()))),
        WhitelistRequired => (format!("Only in rooms I allow is selected for {verb} access, and no Allow rule matches this room or its spaces. Open Room and space access → Add room or space, select this target or one of its spaces, and choose Allow for {verb} access. App permissions cannot bypass this restriction."), ProtectionInspectorAction::Rule(if space { AccessRuleKey::Space(room.into()) } else { AccessRuleKey::Room(room.into()) })),
    };
    Explanation { text: format!("{status}.\n{reason}"), control: Some(control) }
}

fn explain_capability(store: &PermissionStore, manifest: Option<&MiniAppManifest>, subject: &SubjectChoice, cap: &Capability,
    room: &str, space: bool, label: impl Fn(&str) -> String) -> Explanation
{
    let agent = matches!(subject.context, Some(ContextId::Agent { .. }));
    let declares_cap = |cap: &Capability| if agent { super::ai::tools::AI_ROOM_SESSION_CAP_IDS.contains(&cap.id) }
        else { manifest.is_some_and(|manifest| manifest.declares_capability(cap)) };
    let declares_perm = |permission| if agent {
        super::ai::tools::AI_ROOM_SESSION_CAP_IDS.iter().any(|id| capabilities::by_id(id).and_then(|cap| cap.group) == Some(permission))
    } else { manifest.is_some_and(|manifest| manifest.declares(permission)) };
    let evaluation = store.capability_evaluation_for_in_context(&subject.subject, declares_perm, declares_cap, cap,
        PermissionContext { origin_room: subject.origin_room.as_deref(), target_room: Some(room) });
    if !agent && manifest.is_none() {
        return Explanation {
            text: "App permission: unavailable. This mini-app is no longer installed. Its previously accessed data is still protected. Install it again to change its app permissions.".into(),
            control: None,
        };
    }
    explain_capability_result(evaluation, subject, cap, room, space, label)
}

fn explain_capability_result(evaluation: CapabilityEvaluation<'_>, subject: &SubjectChoice, cap: &Capability,
    room: &str, space: bool, label: impl Fn(&str) -> String) -> Explanation
{
    use CapabilityDecisionReason::*;
    let permission_control = |permission, capability| ProtectionInspectorAction::Ability {
        subject: subject.subject.clone(), permission, capability,
        scope: if space { RoomScope::Selection { rooms: Vec::new(), spaces: vec![room.into()] } } else { RoomScope::room(room) },
    };
    let default_control = cap.group.map(|permission| permission_control(permission, Some(cap.id.into())));
    let (reason, control) = match evaluation.reason {
        Unavailable => ("This ability is not available in this build; permission settings cannot enable it.".into(), None),
        Undeclared => ("This app or agent does not support this ability. Permission settings cannot add abilities it has not declared.".into(), Some(ProtectionInspectorAction::SubjectInfo(subject.subject.clone()))),
        SubjectRestricted => ("Robrix restricted this app or agent after a security failure. Review the restriction in its settings before allowing it to run again.".into(), Some(ProtectionInspectorAction::SubjectInfo(subject.subject.clone()))),
        RoomPolicy { access, reason } => {
            let explanation = explain_room(RoomPolicyEvaluation { decision: if evaluation.effective == Effective::Granted { PolicyDecision::Allow } else { PolicyDecision::Deny }, reason }, access, room, space, label);
            (explanation.text, explanation.control)
        }
        PermissionDenied { permission } => (format!("The {} permission group is blocked for this app or agent. Blocking a group overrides individual abilities and room allowances. Open that permission group and choose Use default to remove the block. Then add an allowance if approval is required.", permission.title()), Some(permission_control(permission, None))),
        CapabilityDenied => ("This individual ability is blocked. Open its permission setting and choose Use default to remove the block. Then add an allowance if approval is required.".into(), default_control),
        CapabilityGrant => ("You have allowed this individual ability.".into(), default_control),
        ScopedGrant => ("A saved or temporary permission allows this ability for this target from the app or agent’s room.".into(), default_control),
        RoomGrant => ("An earlier per-room allowance matches this target. Manage the app or agent's saved room allowances to revoke it.".into(), default_control),
        PermissionGrant => ("The permission group allows this action for this app or agent.".into(), cap.group.map(|permission| permission_control(permission, None))),
        NormalPermission => ("This basic ability is allowed by the current permission defaults. Strict mode or a more specific rule can require approval.".into(), default_control),
        ApprovalRequired => ("This action needs your approval. Change the app or agent’s permission for this room, or approve its request when prompted.".into(), default_control),
        NoPermissionRequired => ("This ability does not need a separate app permission. Room access and data-sharing checks still apply.".into(), None),
    };
    let status = match evaluation.effective {
        Effective::Granted => "App permission: allowed",
        Effective::NeedsPrompt => "App permission: approval required",
        Effective::Denied => "App permission: blocked",
        Effective::Undeclared => "App permission: unavailable or unsupported",
    };
    let applicability = if cap.scope == CapabilityScope::Room && subject.origin_room.as_deref() != Some(room) {
        "\nThis ability works only in the app or agent’s attached room. Choose that room, or check an ability that supports other rooms."
    } else if matches!(cap.scope, CapabilityScope::MultiRoom | CapabilityScope::Space) {
        "\nThis result applies to the selected target. Requests involving several rooms check each room separately and may return only the allowed rooms."
    } else { "" };
    Explanation { text: format!("{} ({})\n{status}.\n{reason}{applicability}\nData sharing and actions influenced by outside content may still need separate approval. Other requirements of the operation also apply.", cap.title, cap.id), control }
}

fn retained_text(snapshots: &[ContextSnapshot], account: &str, room: &str, label: impl Fn(&ContextId) -> String) -> String {
    let source = Source::Room { account: account.into(), room: room.into() };
    let mut lines = Vec::new();
    for snapshot in snapshots {
        if snapshot.context.account() != account { continue; }
        if snapshot.label.contains(&source) {
            lines.push(format!("• {} — may include data from this room", label(&snapshot.context)));
        } else if snapshot.label.contains(&Source::UnknownPrivate) {
            lines.push(format!("• {} — may include private data whose room is unknown", label(&snapshot.context)));
        }
    }
    if lines.is_empty() { "No app data or agent history was recorded as possibly containing data from this room, or private data with an unknown source, when last checked.".into() }
    else { lines.join("\n") }
}

fn retained_code_text(apps: &[(String, String, Result<Label, String>)], account: &str, room: &str) -> String {
    let source = Source::Room { account: account.into(), room: room.into() };
    let mut lines = Vec::new();
    let mut missing = 0;
    for (app, name, labels) in apps {
        match labels {
            Ok(labels) if labels.contains(&source) => lines.push(format!("• {name} ({app}) — its code may include data from this room, even without a running instance")),
            Ok(labels) if labels.contains(&Source::UnknownPrivate) => lines.push(format!("• {name} ({app}) — its code may include private data whose room is unknown")),
            Err(_) => missing += 1,
            _ => {}
        }
    }
    if lines.is_empty() { lines.push("No installed mini-app code was recorded as possibly containing data from this room, or private data with an unknown source, when last checked.".into()); }
    if missing > 0 { lines.push(format!("The source of data in {missing} installed mini-app(s) could not be checked. They may still know data from this room.")); }
    lines.join("\n")
}

impl ProtectionInspectorRef {
    pub fn configure(&self, cx: &mut Cx) {
        if let Some(mut inner) = self.borrow_mut() { inner.configure(cx); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2app_core::permissions::{GrantState, RoomPolicyMode};

    #[test]
    fn changing_accounts_discards_stale_control_navigation() {
        struct ResetAccount(Option<String>);
        impl Drop for ResetAccount {
            fn drop(&mut self) {
                crate::a2app::information_flow::TEST_ACCOUNT.with(|account| account.replace(self.0.take()));
            }
        }
        let _account = ResetAccount(crate::a2app::information_flow::TEST_ACCOUNT.with(|account|
            account.replace(Some("@inspector-new:example.org".into()))));
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let widget = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            makepad_code_editor::script_mod(vm);
            crate::shared::script_mod(vm);
            crate::a2app::permission_choices::script_mod(vm);
            crate::a2app::permission_prompt::script_mod(vm);
            super::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.ProtectionInspector {} });
            WidgetRef::script_from_value(vm, value)
        });
        let mut inspector = widget.borrow_mut::<ProtectionInspector>().unwrap();
        inspector.account = "@inspector-old:example.org".into();
        inspector.capability_control = Some(ProtectionInspectorAction::Rule(AccessRuleKey::Room("!old:example.org".into())));
        inspector.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 1);
        let uid = inspector.view.button(&cx, ids!(capability_rule)).widget_uid();
        let click = cx.capture_actions(|cx| cx.widget_action(uid, ButtonAction::Clicked(Default::default())));
        let actions = cx.capture_actions(|cx| inspector.handle_event(cx, &Event::Actions(click), &mut Scope::empty()));
        assert!(!actions.iter().any(|action| action.downcast_ref::<ProtectionInspectorAction>().is_some()));
        assert_eq!(inspector.account, "@inspector-new:example.org");
        assert_eq!(inspector.view.permission_choices(&cx, ids!(page_choice)).selected_item(), 0);
        assert!(inspector.selected_subject(&cx).is_none_or(|subject|
            subject.context.as_ref().is_none_or(|context| context.account() == "@inspector-new:example.org")));
    }

    #[test]
    fn inspector_guides_each_choice_and_discards_replaced_control_buttons() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let widget = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            makepad_code_editor::script_mod(vm);
            crate::shared::script_mod(vm);
            crate::a2app::permission_choices::script_mod(vm);
            crate::a2app::permission_prompt::script_mod(vm);
            super::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.ProtectionInspector {} });
            WidgetRef::script_from_value(vm, value)
        });
        let mut editor = widget.borrow_mut::<ProtectionInspector>().unwrap();
        editor.account = super::super::information_flow::account().unwrap_or_default();
        editor.targets = vec![("!target:example.org".into(), "Target room".into(), false)];
        editor.subjects = vec![agent()];
        editor.update_page(&mut cx);
        assert!(editor.view.widget(&cx, ids!(target_picker)).visible());
        assert!(!editor.view.widget(&cx, ids!(room_page)).visible());
        let uid = editor.view.drop_down(&cx, ids!(target)).widget_uid();
        let actions = cx.capture_actions(|cx| cx.widget_action(uid, DropDownAction::Select(1)));
        editor.handle_event(&mut cx, &Event::Actions(actions), &mut Scope::empty());
        assert!(!editor.view.widget(&cx, ids!(target_picker)).visible());
        assert!(editor.view.widget(&cx, ids!(room_page)).visible());
        assert!(!editor.view.widget(&cx, ids!(retained_page)).visible());

        editor.read_controls = vec![ProtectionInspectorAction::Rule(AccessRuleKey::Room("!target:example.org".into()))];
        editor.populate_controls(&mut cx);
        let uid = editor.view.view(&cx, ids!(read_controls_section)).borrow().unwrap().children[0].1.widget_uid();
        editor.read_controls = vec![ProtectionInspectorAction::Global];
        editor.populate_controls(&mut cx);
        let clicks = cx.capture_actions(|cx| cx.widget_action(uid, ButtonAction::Clicked(Default::default())));
        let actions = cx.capture_actions(|cx| editor.handle_event(cx, &Event::Actions(clicks), &mut Scope::empty()));
        assert!(!actions.iter().any(|action| action.downcast_ref::<ProtectionInspectorAction>().is_some()));

        editor.view.permission_choices(&cx, ids!(page_choice)).set_selected_item(&mut cx, 1);
        editor.update_page(&mut cx);
        assert!(!editor.view.widget(&cx, ids!(room_page)).visible());
        assert!(editor.view.widget(&cx, ids!(subject_page)).visible());
        assert!(editor.view.widget(&cx, ids!(subject_picker)).visible());
        assert!(!editor.view.widget(&cx, ids!(subject_check)).visible());
        let uid = editor.view.drop_down(&cx, ids!(subject)).widget_uid();
        let actions = cx.capture_actions(|cx| cx.widget_action(uid, DropDownAction::Select(1)));
        editor.handle_event(&mut cx, &Event::Actions(actions), &mut Scope::empty());
        assert!(!editor.view.widget(&cx, ids!(subject_picker)).visible());
        assert!(editor.view.widget(&cx, ids!(subject_check)).visible());
        assert!(!editor.view.widget(&cx, ids!(subject_context)).visible());
        let uid = editor.view.button(&cx, ids!(change_target)).widget_uid();
        let actions = cx.capture_actions(|cx| cx.widget_action(uid, ButtonAction::Clicked(Default::default())));
        editor.handle_event(&mut cx, &Event::Actions(actions), &mut Scope::empty());
        assert!(editor.view.widget(&cx, ids!(target_picker)).visible());
        assert!(!editor.view.widget(&cx, ids!(subject_page)).visible());
        assert_eq!(editor.selected_target(&cx).map(|target| target.0.as_str()), Some("!target:example.org"));
    }

    fn agent() -> SubjectChoice {
        SubjectChoice {
            subject: agent_subject("!agent:example.org"), label: "Test agent".into(),
            context: Some(ContextId::Agent { account: "@alice:example.org".into(), room: "!agent:example.org".into() }),
            origin_room: Some("!agent:example.org".into()), active: true,
        }
    }

    #[test]
    fn room_allowance_does_not_hide_a_blocked_app_permission() {
        let mut permissions = PermissionStore::default();
        let subject = agent();
        let cap = capabilities::by_id("matrix.rooms.messages.read").unwrap();
        permissions.set_room_policy("!private:example.org", RoomAccess::Read, PolicyDecision::Allow);
        permissions.set(&subject.subject, Permission::MatrixRoomsRead, GrantState::Denied);
        let explanation = explain_capability(&permissions, None, &subject, cap, "!private:example.org", false, str::to_owned);
        assert!(explanation.text.contains("App permission: blocked"));
        assert!(explanation.text.contains("permission group is blocked"));
        assert!(matches!(explanation.control, Some(ProtectionInspectorAction::Ability {
            permission: Permission::MatrixRoomsRead, capability: None, ..
        })));
        permissions.set(&subject.subject, Permission::MatrixRoomsRead, GrantState::Ask);
        permissions.set_capability(&subject.subject, cap.id, GrantState::Denied);
        let explanation = explain_capability(&permissions, None, &subject, cap, "!private:example.org", false, str::to_owned);
        assert!(explanation.text.contains("This individual ability is blocked"));
        assert!(matches!(explanation.control, Some(ProtectionInspectorAction::Ability {
            capability: Some(id), ..
        }) if id == cap.id));
    }

    #[test]
    fn reason_links_identify_master_ancestor_space_and_missing_allowlist_rule() {
        let mut permissions = PermissionStore::default();
        let target = "!private:example.org";
        let reason = explain_room(permissions.room_policy_evaluation(Some(target), RoomAccess::Write), RoomAccess::Write, target, false, str::to_owned);
        assert!(reason.text.contains("Allow room writes is off"));
        assert!(matches!(reason.control, Some(ProtectionInspectorAction::Global)));
        permissions.set_room_spaces(target, vec!["!protected:example.org".into()]);
        permissions.set_space_policy("!protected:example.org", RoomAccess::Read, PolicyDecision::Deny);
        let reason = explain_room(permissions.room_policy_evaluation(Some(target), RoomAccess::Read), RoomAccess::Read, target, false, str::to_owned);
        assert!(reason.text.contains("ancestor space !protected:example.org"));
        assert!(matches!(reason.control, Some(ProtectionInspectorAction::Rule(AccessRuleKey::Space(id))) if id == "!protected:example.org"));
        permissions.set_space_policy("!protected:example.org", RoomAccess::Read, PolicyDecision::Ask);
        permissions.set_policy_mode(RoomAccess::Read, RoomPolicyMode::WhitelistOnly);
        let reason = explain_room(permissions.room_policy_evaluation(Some(target), RoomAccess::Read), RoomAccess::Read, target, false, str::to_owned);
        assert!(reason.text.contains("no Allow rule matches this room or its spaces"));
        assert!(matches!(reason.control, Some(ProtectionInspectorAction::Rule(AccessRuleKey::Room(id))) if id == target));
    }

    #[test]
    fn inspector_lists_every_matching_block_without_changing_enforcement_order() {
        let mut permissions = PermissionStore::default();
        let target = "!private:example.org";
        permissions.set_room_spaces(target, vec!["!protected:example.org".into(), "!outer:example.org".into()]);
        permissions.set_room_policy(target, RoomAccess::Write, PolicyDecision::Deny);
        permissions.set_space_policy("!protected:example.org", RoomAccess::Write, PolicyDecision::Deny);
        permissions.set_space_policy("!outer:example.org", RoomAccess::Write, PolicyDecision::Deny);
        let (text, controls) = explain_room_rules(&permissions, RoomAccess::Write, target, false, str::to_owned);
        assert!(text.contains("Allow room writes is off"));
        assert!(text.contains(target));
        assert!(text.contains("!protected:example.org"));
        assert!(text.contains("!outer:example.org"));
        assert_eq!(controls.len(), 4);
        assert!(matches!(controls.first(), Some(ProtectionInspectorAction::Global)));
        assert_eq!(permissions.room_policy_evaluation(Some(target), RoomAccess::Write).reason, RoomPolicyReason::WriteMasterOff);
    }

    #[test]
    fn retained_readers_include_stopped_contexts_and_unknown_sources_but_not_other_accounts() {
        let account = "@alice:example.org";
        let room = "!private:example.org";
        let snapshot = |app: &str, account: &str, label| ContextSnapshot {
            context: ContextId::App { account: account.into(), app: app.into(), room: None },
            epoch: 0, label, clearance: None, influences: Default::default(),
        };
        let snapshots = vec![
            snapshot("stopped", account, [Source::Room { account: account.into(), room: room.into() }].into()),
            snapshot("legacy", account, [Source::UnknownPrivate].into()),
            snapshot("other-account", "@bob:example.org", [Source::Room { account: account.into(), room: room.into() }].into()),
        ];
        let text = retained_text(&snapshots, account, room, |context| context.app().unwrap().into());
        assert!(text.contains("stopped — may include data from this room"));
        assert!(text.contains("legacy — may include private data whose room is unknown"));
        assert!(!text.contains("other-account"));
    }

    #[test]
    fn private_generated_code_is_visible_without_any_context_snapshot() {
        let account = "@alice:example.org";
        let room = "!private:example.org";
        let apps = vec![("unlaunched".into(), "New mini-app".into(), Ok([Source::Room { account: account.into(), room: room.into() }].into()))];
        let text = retained_code_text(&apps, account, room);
        assert!(text.contains("New mini-app (unlaunched)"));
        assert!(text.contains("even without a running instance"));
        assert!(!retained_code_text(&apps, "@other:example.org", room).contains("New mini-app"));
    }

    #[test]
    fn inspector_widget_routes_the_deciding_ancestor_rule_to_the_room_editor() {
        use crate::a2app::permission_prompt::PermissionScopeEditorWidgetRefExt;
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let (inspector, screen) = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            makepad_code_editor::script_mod(vm);
            crate::shared::script_mod(vm);
            crate::a2app::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.ProtectionInspector {} });
            let inspector = WidgetRef::script_from_value(vm, value);
            let value = script_eval!(vm, { mod.widgets.MiniAppsScreen {} });
            (inspector, WidgetRef::script_from_value(vm, value))
        });
        let mut editor = inspector.borrow_mut::<ProtectionInspector>().unwrap();
        let space = "!ancestor:example.org";
        editor.read_controls = explain_room(RoomPolicyEvaluation {
            decision: PolicyDecision::Deny, reason: RoomPolicyReason::SpaceRule { space, ancestor: true },
        }, RoomAccess::Read, "!target:example.org", false, str::to_owned).control.into_iter().collect();
        editor.targets.push(("!target:example.org".into(), "Target room".into(), false));
        editor.selected_target_id = Some("!target:example.org".into());
        editor.populate_controls(&mut cx);
        let uid = editor.view.view(&cx, ids!(read_controls_section)).borrow().unwrap().children[0].1.widget_uid();
        let click = cx.capture_actions(|cx| cx.widget_action(uid, ButtonAction::Clicked(Default::default())));
        let actions = cx.capture_actions(|cx| editor.handle_event(cx, &Event::Actions(click), &mut Scope::empty()));
        assert!(actions.iter().any(|action| matches!(action.downcast_ref::<ProtectionInspectorAction>(),
            Some(ProtectionInspectorAction::Rule(AccessRuleKey::Space(id))) if id == space)));
        screen.handle_event(&mut cx, &Event::Actions(actions), &mut Scope::empty());
        let selection = screen.permission_scope_editor(&cx, ids!(access_scope)).selection().unwrap();
        assert_eq!(selection.scope, a2app_core::permissions::RoomScope::Selection { rooms: vec![], spaces: vec![space.into()] });
        let target = RoomScope::room("!different-target:example.org");
        let actions = cx.capture_actions(|cx| cx.action(ProtectionInspectorAction::Ability {
            subject: agent().subject, permission: Permission::MatrixRoomsRead,
            capability: Some("matrix.rooms.messages.read".into()), scope: target.clone(),
        }));
        screen.handle_event(&mut cx, &Event::Actions(actions), &mut Scope::empty());
        let selection = screen.permission_scope_editor(&cx, ids!(access_scope)).selection().unwrap();
        assert_eq!(selection.scope, target, "the permission editor must keep the inspected target, independently of the agent's attached room");
    }
}
