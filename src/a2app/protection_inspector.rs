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
use super::runtime::with_a2app;

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    mod.widgets.ProtectionInspector = set_type_default() do #(ProtectionInspector::register_widget(vm)) {
        width: Fill, height: Fill, flow: Down
        content := ScrollYView {
            width: Fill, height: Fill, flow: Down, spacing: 10, padding: 15
            mod.widgets.PermissionOptionLabel {
                text: "Inspect the current room policies and a particular app or agent's permission checks. A permission allowance still requires data-sharing checks, action approval and the actual operation's other checks."
            }
            refresh := RobrixNeutralIconButton {
                padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}
                text: "Refresh protection details"
            }
            snapshot_status := mod.widgets.PermissionOptionLabel {}
            SubsectionLabel { text: "Room or space", margin: 0 }
            target := mod.widgets.PermissionDropDown {}
            target_details := mod.widgets.PermissionOptionLabel {}
            SubsectionLabel { text: "Read policy", margin: 0 }
            read_details := mod.widgets.PermissionOptionLabel {}
            read_control_choice := mod.widgets.PermissionDropDown {}
            read_rule := RobrixNeutralIconButton {
                padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}
                text: "Open deciding read control"
            }
            SubsectionLabel { text: "Write policy", margin: 0 }
            write_details := mod.widgets.PermissionOptionLabel {}
            write_control_choice := mod.widgets.PermissionDropDown {}
            write_rule := RobrixNeutralIconButton {
                padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}
                text: "Open deciding write control"
            }
            SubsectionLabel { text: "App or agent context", margin: 0 }
            subject := mod.widgets.PermissionDropDown {}
            subject_details := mod.widgets.PermissionOptionLabel {}
            SubsectionLabel { text: "Ability", margin: 0 }
            capability := mod.widgets.PermissionDropDown {}
            capability_details := mod.widgets.PermissionOptionLabel {}
            capability_rule := RobrixNeutralIconButton {
                padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}
                text: "Open deciding permission control"
            }
            SubsectionLabel { text: "Retained room provenance", margin: 0 }
            retained_details := mod.widgets.PermissionOptionLabel {}
            mod.widgets.PermissionOptionLabel {
                text: "Provenance records which sources a context may know, including saved data, history and app code. They do not prove that original messages are still stored. Blocking new reads does not erase prior provenance or revoke sharing rules."
            }
            sharing := RobrixNeutralIconButton {
                padding: 8, icon_walk: Walk{width: 0, height: 0, margin: 0}
                text: "Inspect data sharing and sensitive actions"
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
}

impl Widget for ProtectionInspector {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        let Event::Actions(actions) = event else { return };
        if self.view.button(cx, ids!(refresh)).clicked(actions) {
            self.configure(cx);
        } else if self.view.drop_down(cx, ids!(target)).changed(actions).is_some()
            || self.view.drop_down(cx, ids!(subject)).changed(actions).is_some()
            || self.view.drop_down(cx, ids!(capability)).changed(actions).is_some()
        {
            self.update_details(cx);
        }
        for (button, control) in [
            (ids!(read_rule), self.read_controls.get(self.view.drop_down(cx, ids!(read_control_choice)).selected_item())),
            (ids!(write_rule), self.write_controls.get(self.view.drop_down(cx, ids!(write_control_choice)).selected_item())),
            (ids!(capability_rule), self.capability_control.as_ref()),
        ] {
            if self.view.button(cx, button).clicked(actions) && let Some(control) = control {
                cx.action(control.clone());
            }
        }
        if self.view.button(cx, ids!(sharing)).clicked(actions) {
            cx.action(ProtectionInspectorAction::Sharing);
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)
    }
}

impl ProtectionInspector {
    fn configure(&mut self, cx: &mut Cx) {
        let previous_target = self.selected_target(cx).map(|target| target.0.clone());
        let previous_subject = self.selected_subject(cx).cloned();
        self.account = super::information_flow::account().unwrap_or_default();
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
        self.view.drop_down(cx, ids!(target)).set_labels(cx, self.targets.iter().map(|(id, name, space)|
            format!("{}: {name} ({id})", if *space { "Space" } else { "Room" })).collect());
        self.view.drop_down(cx, ids!(target)).set_selected_item(cx,
            previous_target.and_then(|id| self.targets.iter().position(|target| target.0 == id)).unwrap_or(0));

        self.subjects = self.snapshots.iter().map(|snapshot| {
            let context = &snapshot.context;
            let subject = context.app().map(str::to_owned).unwrap_or_else(|| agent_subject(context.room().unwrap_or_default()));
            SubjectChoice {
                subject, label: format!("{} · {}", self.context_label(context, &apps), if snapshot.epoch == 0 { "Stopped; provenance retained" } else { "Active" }),
                context: Some(context.clone()), origin_room: context.room().map(str::to_owned), active: snapshot.epoch != 0,
            }
        }).collect();
        for (app, name, scope) in &apps {
            if !self.subjects.iter().any(|subject| subject.subject == *app) {
                self.subjects.push(SubjectChoice {
                    subject: app.clone(), label: format!("{name} ({app}) · No retained context"), context: None, active: false,
                    origin_room: match scope { A2AppScope::Room { room_id } => Some(room_id.clone()), _ => None },
                });
            }
        }
        self.view.drop_down(cx, ids!(subject)).set_labels(cx, self.subjects.iter().map(|subject| subject.label.clone()).collect());
        self.view.drop_down(cx, ids!(subject)).set_selected_item(cx,
            previous_subject.and_then(|previous| self.subjects.iter().position(|subject|
                subject.subject == previous.subject && subject.context == previous.context)).unwrap_or(0));
        let previous_capability = if self.configured { self.view.drop_down(cx, ids!(capability)).selected_item() }
            else { capabilities::CATALOG.iter().position(|cap| cap.id == "matrix.rooms.messages.read").unwrap_or(0) };
        self.view.drop_down(cx, ids!(capability)).set_labels(cx, capabilities::CATALOG.iter().map(|cap| format!("{} ({})", cap.title, cap.id)).collect());
        self.view.drop_down(cx, ids!(capability)).set_selected_item(cx, previous_capability);
        self.configured = true;
        self.view.label(cx, ids!(snapshot_status)).set_text(cx, "Snapshot refreshed. Refresh again after another app, agent or room changes; selecting a target or ability rechecks its current permissions.");
        self.update_details(cx);
    }

    fn selected_target(&self, cx: &Cx) -> Option<&(String, String, bool)> {
        self.targets.get(self.view.drop_down(cx, ids!(target)).selected_item())
    }

    fn selected_subject(&self, cx: &Cx) -> Option<&SubjectChoice> {
        self.subjects.get(self.view.drop_down(cx, ids!(subject)).selected_item())
    }

    fn room_label(&self, room: &str) -> String {
        self.targets.iter().find(|target| target.0 == room).map(|target| format!("{} ({room})", target.1)).unwrap_or_else(|| room.into())
    }

    fn context_label(&self, context: &ContextId, apps: &[(String, String, A2AppScope)]) -> String {
        let app_name = |app: &str| apps.iter().find(|candidate| candidate.0 == app)
            .map(|candidate| format!("{} ({app})", candidate.1)).unwrap_or_else(|| app.into());
        match context {
            ContextId::App { app, room, .. } => format!("{} · {}", app_name(app), room.as_deref().map(|room| self.room_label(room)).unwrap_or_else(|| "Account context".into())),
            ContextId::PublicApp { app, .. } => format!("{} · Public context", app_name(app)),
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
            self.view.label(cx, ids!(capability_details)).set_text(cx, capability.as_ref().map(|result| result.text.as_str()).unwrap_or("Select an installed app or an agent context and an ability."));
            self.capability_control = capability.and_then(|result| result.control);
        } else {
            for id in [ids!(read_details), ids!(write_details), ids!(capability_details)] {
                self.view.label(cx, id).set_text(cx, "No permission result is available. Select a room or space after signing in.");
            }
        }
        self.view.label(cx, ids!(target_details)).set_text(cx, if target.as_ref().is_some_and(|target| target.2) {
            "These checks apply to the space itself. Its rules also cover nested rooms; select a child room to inspect its own blocks and other ancestor spaces."
        } else { "Read and write policies below are host restrictions for this exact room, before app or agent grants." });
        let context_text = subject.as_ref().map(|subject| {
            let activity = if subject.active { "Active at the last refresh." } else { "Not active at the last refresh. A new activation must pass its own session and data-sharing checks." };
            let clearance = if matches!(subject.context, Some(ContextId::PublicApp { .. })) {
                "Public context: private room and account inputs are blocked by its clearance even when a permission is allowed."
            } else { "Data-sharing checks and sensitive-action approval are separate from the permission result below." };
            format!("{}\n{activity}\n{clearance}", subject.label)
        }).unwrap_or_else(|| "No app or agent context is available.".into());
        self.view.label(cx, ids!(subject_details)).set_text(cx, &context_text);
        let retained = match (&self.provenance_error, &target) {
            (Some(error), _) => format!("Retained provenance is unavailable: {error}. No conclusion about retained room data can be drawn."),
            (_, Some((room, _, _))) => format!("Context provenance:\n{}\n\nShared mini-app source provenance:\n{}",
                retained_text(&self.snapshots, &self.account, room, |context|
                    self.subjects.iter().find(|subject| subject.context.as_ref() == Some(context)).map(|subject| subject.label.clone()).unwrap_or_else(|| format!("{context:?}"))),
                retained_code_text(&self.code_sources, &self.account, room)),
            _ => "Select a room or space to inspect retained provenance.".into(),
        };
        self.view.label(cx, ids!(retained_details)).set_text(cx, &retained);
        for (choice, button, controls) in [
            (ids!(read_control_choice), ids!(read_rule), &self.read_controls),
            (ids!(write_control_choice), ids!(write_rule), &self.write_controls),
        ] {
            self.view.drop_down(cx, choice).set_labels(cx, controls.iter().map(|control| match control {
                ProtectionInspectorAction::Global => "Global policy / write switch".into(),
                ProtectionInspectorAction::Rule(AccessRuleKey::Room(room)) => format!("Room: {}", self.room_label(room)),
                ProtectionInspectorAction::Rule(AccessRuleKey::Space(space)) => format!("Space: {}", self.room_label(space)),
                _ => "Permission control".into(),
            }).collect());
            self.view.drop_down(cx, choice).set_selected_item(cx, 0);
            self.view.widget(cx, choice).set_visible(cx, controls.len() > 1);
            self.view.widget(cx, button).set_visible(cx, !controls.is_empty());
        }
        self.view.widget(cx, ids!(capability_rule)).set_visible(cx, self.capability_control.is_some());
        self.view.redraw(cx);
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
    let mut lines = vec!["Blocked by host room policy. Every blocking rule below must be resolved before access is possible:".into()];
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
        PolicyDecision::Allow => "Allowed by host room policy",
        PolicyDecision::Ask => "No host room allowance; app or agent permission checks decide",
        PolicyDecision::Deny => "Blocked by host room policy",
    };
    let (reason, control) = match evaluation.reason {
        WriteMasterOff => ("The top-level room-write switch is off. Enable it on the Mini Apps main screen to restore saved write rules.".into(), ProtectionInspectorAction::Global),
        GlobalBlock => (format!("The global {verb} default blocks every room. Change it on the Mini Apps main screen; a room Allow cannot override it."), ProtectionInspectorAction::Global),
        GlobalDefault => (format!("The global {verb} default decides this result. Change that default on the Mini Apps main screen or add a room or space rule."), ProtectionInspectorAction::Global),
        RoomRule { room } => (format!("The {verb} rule for {} decides this result. Edit that room rule; any matching block still wins.", label(room)), ProtectionInspectorAction::Rule(AccessRuleKey::Room(room.into()))),
        SpaceRule { space, ancestor } => (format!("The {verb} rule for {}{} decides this result. Edit that space rule; any matching block still wins.", if ancestor { "ancestor space " } else { "space " }, label(space)), ProtectionInspectorAction::Rule(AccessRuleKey::Space(space.into()))),
        UnresolvedSpaceHierarchy { space, rule } => (format!("Robrix has not resolved whether this target belongs to space {}. Its {} {verb} rule cannot yet be safely evaluated. Wait for room/space synchronization and refresh; inspect that space rule if needed.", label(space), if rule == PolicyDecision::Deny { "Block" } else { "Allow" }), ProtectionInspectorAction::Rule(AccessRuleKey::Space(space.into()))),
        WhitelistRequired => (format!("Allowlist-only {verb} access is enabled, and no room or ancestor-space Allow matches. Add an Allow for this target or one of its spaces; ordinary app grants cannot bypass this restriction."), ProtectionInspectorAction::Rule(if space { AccessRuleKey::Space(room.into()) } else { AccessRuleKey::Room(room.into()) })),
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
            text: "Permission layer: unavailable. The app associated with this retained context is no longer installed. Its stored data and code provenance remain protected; app permission controls require an installed manifest.".into(),
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
        Undeclared => ("This app's manifest or agent's tool catalog does not declare this ability. Permission settings cannot grant an undeclared ability.".into(), Some(ProtectionInspectorAction::SubjectInfo(subject.subject.clone()))),
        SubjectRestricted => ("Robrix restricted this app or agent after a security failure. Review the restriction in its settings before allowing it to run again.".into(), Some(ProtectionInspectorAction::SubjectInfo(subject.subject.clone()))),
        RoomPolicy { access, reason } => {
            let explanation = explain_room(RoomPolicyEvaluation { decision: if evaluation.effective == Effective::Granted { PolicyDecision::Allow } else { PolicyDecision::Deny }, reason }, access, room, space, label);
            (explanation.text, explanation.control)
        }
        PermissionDenied { permission } => (format!("The {} permission group is blocked for this app or agent. Its group block overrides capability and scoped allowances; change that group control.", permission.title()), Some(permission_control(permission, None))),
        CapabilityDenied => ("This individual ability is blocked. Change its app or agent capability control.".into(), default_control),
        CapabilityGrant => ("An explicit allowance for this ability passes the permission layer.".into(), default_control),
        ScopedGrant => ("A saved or session allowance matches this ability, target and origin room.".into(), default_control),
        RoomGrant => ("An earlier per-room allowance matches this target. Manage the app or agent's saved room allowances to revoke it.".into(), default_control),
        PermissionGrant => ("The app or agent's permission group allowance passes this permission check.".into(), cap.group.map(|permission| permission_control(permission, None))),
        NormalPermission => ("This declared normal-tier ability is enabled by the current default. Strict mode or narrower rules can require approval.".into(), default_control),
        ApprovalRequired => ("No applicable allowance passes this permission check. Review the app or agent's scoped ability permissions, or approve its request when prompted.".into(), default_control),
        NoPermissionRequired => ("This ability does not require an app permission group. Its host and data-flow checks still apply.".into(), None),
    };
    let status = match evaluation.effective {
        Effective::Granted => "Permission layer: allowed",
        Effective::NeedsPrompt => "Permission layer: approval required",
        Effective::Denied => "Permission layer: blocked",
        Effective::Undeclared => "Permission layer: unavailable or undeclared",
    };
    let applicability = if cap.scope == CapabilityScope::Room && subject.origin_room.as_deref() != Some(room) {
        "\nThis is an attached-room ability: it cannot target the selected room from this context. Select its attached room or inspect a cross-room ability."
    } else if matches!(cap.scope, CapabilityScope::MultiRoom | CapabilityScope::Space) {
        "\nThis is the check for the selected target. A collection query may start for an allowed subset, but every returned or accessed room must pass its own target check."
    } else { "" };
    Explanation { text: format!("{} ({})\n{status}.\n{reason}{applicability}\nThis result does not authorize data sharing or a sensitive action, and does not promise that an operation will succeed.", cap.title, cap.id), control }
}

fn retained_text(snapshots: &[ContextSnapshot], account: &str, room: &str, label: impl Fn(&ContextId) -> String) -> String {
    let source = Source::Room { account: account.into(), room: room.into() };
    let mut lines = Vec::new();
    for snapshot in snapshots {
        if snapshot.context.account() != account { continue; }
        if snapshot.label.contains(&source) {
            lines.push(format!("• {} — this room's provenance", label(&snapshot.context)));
        } else if snapshot.label.contains(&Source::UnknownPrivate) {
            lines.push(format!("• {} — unknown private provenance; room ownership cannot be determined", label(&snapshot.context)));
        }
    }
    if lines.is_empty() { "No current or retained context in this account is recorded with this room's provenance or unknown private provenance at the last refresh.".into() }
    else { lines.join("\n") }
}

fn retained_code_text(apps: &[(String, String, Result<Label, String>)], account: &str, room: &str) -> String {
    let source = Source::Room { account: account.into(), room: room.into() };
    let mut lines = Vec::new();
    let mut missing = 0;
    for (app, name, labels) in apps {
        match labels {
            Ok(labels) if labels.contains(&source) => lines.push(format!("• {name} ({app}) — shared source contains this room's provenance, even without a running instance")),
            Ok(labels) if labels.contains(&Source::UnknownPrivate) => lines.push(format!("• {name} ({app}) — shared source has unknown private provenance; room ownership cannot be determined")),
            Err(_) => missing += 1,
            _ => {}
        }
    }
    if lines.is_empty() { lines.push("No installed mini-app source with this room's known or unknown private provenance is recorded at the last refresh.".into()); }
    if missing > 0 { lines.push(format!("Code provenance is unavailable for {missing} installed mini-app(s); no conclusion about their retained room data can be drawn.")); }
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
        assert!(explanation.text.contains("Permission layer: blocked"));
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
        assert!(reason.text.contains("top-level room-write switch is off"));
        assert!(matches!(reason.control, Some(ProtectionInspectorAction::Global)));
        permissions.set_room_spaces(target, vec!["!protected:example.org".into()]);
        permissions.set_space_policy("!protected:example.org", RoomAccess::Read, PolicyDecision::Deny);
        let reason = explain_room(permissions.room_policy_evaluation(Some(target), RoomAccess::Read), RoomAccess::Read, target, false, str::to_owned);
        assert!(reason.text.contains("ancestor space !protected:example.org"));
        assert!(matches!(reason.control, Some(ProtectionInspectorAction::Rule(AccessRuleKey::Space(id))) if id == "!protected:example.org"));
        permissions.set_space_policy("!protected:example.org", RoomAccess::Read, PolicyDecision::Ask);
        permissions.set_policy_mode(RoomAccess::Read, RoomPolicyMode::WhitelistOnly);
        let reason = explain_room(permissions.room_policy_evaluation(Some(target), RoomAccess::Read), RoomAccess::Read, target, false, str::to_owned);
        assert!(reason.text.contains("no room or ancestor-space Allow matches"));
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
        assert!(text.contains("top-level room-write switch is off"));
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
        assert!(text.contains("stopped — this room's provenance"));
        assert!(text.contains("legacy — unknown private provenance"));
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
        let uid = editor.view.button(&cx, ids!(read_rule)).widget_uid();
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
