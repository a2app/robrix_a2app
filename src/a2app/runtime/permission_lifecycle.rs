//! User-initiated consent withdrawal and recovery after a denied request.

use makepad_widgets::*;
use a2app_core::information_flow::{self as flow, ContextId, ReaderScope, SharingGrant, Source};
use a2app_core::permissions::{Permission, agent_room_of};
use crate::a2app::information_flow;
use crate::shared::popup_list::{enqueue_popup_notification, PopupKind};
use super::{with_a2app, persistence, publish_grants, stop_app_everywhere};

pub(crate) fn context_matches_subject(context: &ContextId, account: &str, subject: &str) -> bool {
    context.account() == account && match agent_room_of(subject) {
        Some(room) => matches!(context, ContextId::Agent { room: origin, .. } if origin == room),
        None => context.app() == Some(subject),
    }
}

pub(crate) fn sharing_matches_subject(grant: &SharingGrant, account: &str, subject: &str, include_shared: bool) -> bool {
    if !matches!(&grant.source, Source::Account { account: owner } | Source::Room { account: owner, .. } if owner == account) { return false; }
    match &grant.reader {
        ReaderScope::AllReaders => include_shared,
        ReaderScope::App { account: owner, app } => owner == account && app == subject,
        ReaderScope::Context(context) => context_matches_subject(context, account, subject),
    }
}

fn operation_permission(operation: &str) -> Option<Permission> {
    a2app_core::capabilities::by_id(operation).and_then(|capability| capability.group)
        .or_else(|| operation.starts_with("network.").then_some(Permission::Network))
        .or_else(|| (operation == "mcp.tools.call").then_some(Permission::McpTools))
}

fn revoke_flow_permissions(account: &str, subject: &str, permission: Option<Permission>) -> Result<(), String> {
    for grant in flow::effect_authorities()? {
        let operation = grant.operation.strip_prefix("operation:").or_else(|| grant.operation.strip_prefix("action:"));
        let group = operation.and_then(operation_permission)
            .or_else(|| grant.action.as_ref().and_then(|action| operation_permission(&action.kind)));
        if context_matches_subject(&grant.context, account, subject)
            && permission.is_none_or(|permission| group == Some(permission))
        { flow::revoke_effect_authority(grant.id)?; }
    }
    for grant in flow::authorities()? {
        if context_matches_subject(&grant.context, account, subject)
            && permission.is_none_or(|permission| operation_permission(&grant.action.kind) == Some(permission))
        { flow::revoke_authority(grant.id)?; }
    }
    // An app reset must not silently remove a rule shared by other apps.
    // Shared rules remain individually removable in the ordinary panel.
    if permission.is_none() {
        for grant in flow::sharing_grants()? {
            if sharing_matches_subject(&grant, account, subject, false) { flow::revoke_sharing(grant.id)?; }
        }
    }
    Ok(())
}

fn retire_subject(cx: &mut Cx, ui: &WidgetRef, subject: &str) -> Result<(), String> {
    #[cfg(unix)]
    if let Some(room) = agent_room_of(subject).and_then(|room| room.parse().ok()) {
        super::abort_ai_room_work(cx, ui, &room);
        return Ok(());
    }
    #[cfg(unix)]
    super::withdraw_app_tools(subject);
    let background = crate::a2app::background::disable_app_checked(cx, subject);
    stop_app_everywhere(cx, ui, subject);
    let foreground = with_a2app(|state| state.foreground_app.as_deref() == Some(subject)).unwrap_or(false);
    if foreground {
        with_a2app(|state| { state.foreground_app = None; });
        ui.modal(cx, ids!(mini_app_host_modal)).close(cx);
    }
    background
}

fn save_reset(cx: &mut Cx, ui: &WidgetRef, account: &str, result: Result<(), String>, message: &str) {
    publish_grants(cx);
    let saved = with_a2app(|state| {
        let saved = persistence::save_permissions(&state.permissions).map_err(|error| error.to_string());
        state.perms_dirty = saved.is_err();
        saved
    }).unwrap_or_else(|| Err("Mini Apps is unavailable.".into()));
    a2app_core::protection_audit::record_policy_change(account);
    match result.and(saved) {
        Ok(()) => enqueue_popup_notification(message.to_owned(), PopupKind::Success, Some(5.0)),
        Err(error) => enqueue_popup_notification(format!("Couldn't finish saving the permission change: {error}"), PopupKind::Error, Some(8.0)),
    }
    ui.redraw(cx);
}

pub(super) fn reset_app_permissions(cx: &mut Cx, ui: &WidgetRef, subject: &str) {
    let account = match information_flow::account() {
        Ok(account) => account,
        Err(error) => { enqueue_popup_notification(error, PopupKind::Error, Some(6.0)); return; }
    };
    with_a2app(|state| {
        state.permissions.reset_subject(subject);
        state.perms_dirty = true;
        state.dismissed_prompts.retain(|(owner, _)| owner != subject);
        state.dismissed_net_hosts.retain(|(owner, _)| owner != subject);
        state.dismissed_effects.retain(|(context, _)| !context_matches_subject(context, &account, subject));
        state.permission_gestures.retain(|context, _| !context_matches_subject(context, &account, subject));
    });
    publish_grants(cx);
    let result = revoke_flow_permissions(&account, subject, None);
    super::cancel_subject_permission_prompts(cx, ui, subject);
    let retired = retire_subject(cx, ui, subject);
    save_reset(cx, ui, &account, result.and(retired), "Permissions reset. Reopen the app to review new requests. Its saved data is kept.");
}

pub(super) fn ask_again(cx: &mut Cx, ui: &WidgetRef, subject: &str, permission: Permission) {
    let account = match information_flow::account() {
        Ok(account) => account,
        Err(error) => { enqueue_popup_notification(error, PopupKind::Error, Some(6.0)); return; }
    };
    with_a2app(|state| {
        state.permissions.ask_again(subject, permission);
        state.perms_dirty = true;
        state.dismissed_prompts.remove(&(subject.to_string(), permission));
        if permission == Permission::Network { state.dismissed_net_hosts.retain(|(owner, _)| owner != subject); }
        state.dismissed_effects.retain(|(context, _)| !context_matches_subject(context, &account, subject));
        state.permission_gestures.retain(|context, _| !context_matches_subject(context, &account, subject));
    });
    publish_grants(cx);
    let result = revoke_flow_permissions(&account, subject, Some(permission));
    super::cancel_subject_permission_prompts(cx, ui, subject);
    let retired = retire_subject(cx, ui, subject);
    let message = if agent_room_of(subject).is_some() {
        "Saved answers for this permission cleared. Send your agent request again."
    } else { "Saved answers for this permission cleared. Reopen the app and try again." };
    save_reset(cx, ui, &account, result.and(retired), message);
}

pub(super) fn ask_again_capability(cx: &mut Cx, ui: &WidgetRef, subject: &str, capability: &str) {
    if let Some(permission) = a2app_core::capabilities::by_id(capability).and_then(|capability| capability.group) {
        ask_again(cx, ui, subject, permission);
    }
}

/// Ordinary management revocation retires only the owner of that approval.
/// Other apps retain their active work and independent permission choices.
pub(super) fn retire_after_revocation(cx: &mut Cx, ui: &WidgetRef, subject: &str) {
    let account = match information_flow::account() {
        Ok(account) => account,
        Err(error) => { enqueue_popup_notification(error, PopupKind::Error, Some(6.0)); return; }
    };
    with_a2app(|state| {
        state.permissions.clear_tool_denials_for(subject);
        state.dismissed_prompts.retain(|(owner, _)| owner != subject);
        state.dismissed_net_hosts.retain(|(owner, _)| owner != subject);
        state.dismissed_effects.retain(|(context, _)| !context_matches_subject(context, &account, subject));
        state.permission_gestures.retain(|context, _| !context_matches_subject(context, &account, subject));
    });
    publish_grants(cx);
    super::cancel_subject_permission_prompts(cx, ui, subject);
    let retired = retire_subject(cx, ui, subject);
    let message = if agent_room_of(subject).is_some() {
        "Approval removed. Send your agent request again to continue. Other approvals may still apply."
    } else { "Approval removed. Reopen the app to continue. Other approvals may still apply." };
    save_reset(cx, ui, &account, retired, message);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_match_includes_public_and_private_instances_but_is_account_bound() {
        let private = ContextId::App { account: "a".into(), app: "search".into(), room: Some("room".into()) };
        let public = ContextId::PublicApp { account: "a".into(), app: "search".into() };
        let agent = ContextId::Agent { account: "a".into(), room: "room".into() };
        assert!(context_matches_subject(&private, "a", "search"));
        assert!(context_matches_subject(&public, "a", "search"));
        assert!(!context_matches_subject(&private, "b", "search"));
        assert!(!context_matches_subject(&agent, "a", "search"));
        assert!(context_matches_subject(&agent, "a", &a2app_core::permissions::agent_subject("room")));
    }

    #[test]
    fn legacy_operations_map_to_the_permission_that_the_user_can_reset() {
        assert_eq!(operation_permission("network.POST"), Some(Permission::Network));
        assert_eq!(operation_permission("mcp.tools.call"), Some(Permission::McpTools));
        assert_eq!(operation_permission("host.nav.event"), Some(Permission::RobrixNavigation));
        assert_eq!(operation_permission("apps.generate"), Some(Permission::AppGeneration));
        assert_eq!(operation_permission("request:hash"), None);
    }

    #[test]
    fn shared_rules_are_visible_but_subject_reset_cannot_remove_other_apps_consent() {
        let mut grant = SharingGrant {
            id: 1, source: Source::Account { account: "a".into() },
            recipient: flow::Recipient::Clipboard,
            reader: ReaderScope::AllReaders,
            duration: flow::SharingDuration::Permanent,
        };
        assert!(sharing_matches_subject(&grant, "a", "search", true));
        assert!(!sharing_matches_subject(&grant, "a", "search", false));
        assert!(!sharing_matches_subject(&grant, "b", "search", true));
        grant.reader = ReaderScope::App { account: "a".into(), app: "search".into() };
        assert!(sharing_matches_subject(&grant, "a", "search", false));
        assert!(!sharing_matches_subject(&grant, "a", "other", false));
    }

    #[test]
    fn wrong_account_revoke_does_not_remove_saved_flow_approvals() {
        let owner = "@approval-owner:lifecycle-test";
        let context = ContextId::App { account: owner.into(), app: "account-switch-revoke-test".into(), room: Some("!approval:lifecycle-test".into()) };
        flow::register_context(&context).unwrap();
        flow::add_sources(&context, [Source::Account { account: owner.into() }]).unwrap();
        flow::add_influences(&context, [flow::Influence::MiniApp { account: owner.into(), app: "test-source".into() }]).unwrap();
        let sharing = flow::grant_sharing(Source::Account { account: owner.into() }, flow::Recipient::Clipboard,
            ReaderScope::AllReaders, flow::SharingDuration::RobrixSession).unwrap();
        let action = flow::grant_authority_for_activation(&context,
            flow::SensitiveAction { kind: "clipboard.write".into(), target: "clipboard".into() },
            flow::AuthoritySession::RobrixSession, &flow::influences(&context).unwrap(), flow::context_epoch(&context).unwrap()).unwrap();
        let recipient = flow::Recipient::network_origin("https://lifecycle-test.example").unwrap();
        let review = flow::prepare_effect_for_activation(&context, flow::context_epoch(&context).unwrap(), Some(&recipient), None,
            &serde_json::json!({ "operation": "network.http", "url": "https://lifecycle-test.example/page" })).unwrap();
        flow::approve_effect_session(&review, flow::SharingDuration::RobrixSession).unwrap();
        let effect = flow::effect_authorities().unwrap().into_iter().find(|grant| grant.context == context).unwrap().id;
        let previous_account = information_flow::TEST_ACCOUNT.with(|account| account.replace(Some("@another-account:lifecycle-test".into())));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut cx = Cx::new(Box::new(|_, _| {}));
            let ui = WidgetRef::empty();
            super::super::apply_op(&mut cx, &ui, super::super::A2AppOp::RevokeFlowSharing(sharing));
            super::super::apply_op(&mut cx, &ui, super::super::A2AppOp::RevokeFlowAuthority(action));
            super::super::apply_op(&mut cx, &ui, super::super::A2AppOp::RevokeEffectAuthority(effect));
            assert!(flow::sharing_grants().unwrap().iter().any(|grant| grant.id == sharing));
            assert!(flow::authorities().unwrap().iter().any(|grant| grant.id == action));
            assert!(flow::effect_authorities().unwrap().iter().any(|grant| grant.id == effect));
        }));
        information_flow::TEST_ACCOUNT.with(|account| { account.replace(previous_account); });
        flow::revoke_sharing(sharing).unwrap();
        flow::revoke_authority(action).unwrap();
        flow::revoke_effect_authority(effect).unwrap();
        flow::remove_context(&context).unwrap();
        if let Err(error) = result { std::panic::resume_unwind(error); }
    }
}
