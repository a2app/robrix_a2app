//! Recheck room safety at the worker boundary and before delivering results.

use std::sync::{LazyLock, RwLock};

use a2app_core::permissions::{Effective, PermissionContext, PermissionStore, PolicyDecision, RoomAccess};
use a2app_core::information_flow::{self as flow, ContextId, Recipient, SensitiveAction};

pub const ROOM_ACCESS_DENIED: &str = "room access is blocked by the safety rules in Mini Apps";

static POLICY: LazyLock<RwLock<PermissionStore>> = LazyLock::new(Default::default);
static POLICY_REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The UI publishes after changing permissions or room ancestry. Workers read
/// a current snapshot at each boundary, rather than retaining old approvals.
pub fn publish_permission_policy(store: &PermissionStore) {
    publish_permission_policy_at_revision(store, super::spaces::policy_spaces_revision());
}

pub fn publish_permission_policy_at_revision(store: &PermissionStore, revision: u64) {
    let mut policy = POLICY.write().unwrap();
    let current = super::spaces::policy_spaces_revision();
    *policy = store.clone();
    if revision != current { policy.clear_room_spaces(); }
    POLICY_REVISION.store(current, std::sync::atomic::Ordering::SeqCst);
}

pub fn invalidate_room_spaces() {
    POLICY.write().unwrap().clear_room_spaces();
}

pub fn configured_space_ids() -> std::collections::BTreeSet<String> {
    POLICY.read().unwrap().configured_space_ids()
}

fn with_current_policy<T>(f: impl FnOnce(&PermissionStore) -> T) -> T {
    let policy = POLICY.read().unwrap();
    if POLICY_REVISION.load(std::sync::atomic::Ordering::SeqCst) != super::spaces::policy_spaces_revision() {
        // A sync callback can advance the revision before it acquires the
        // write lock. Never use old ancestry in that short interval.
        let mut unresolved = policy.clone();
        unresolved.clear_room_spaces();
        f(&unresolved)
    } else { f(&policy) }
}

/// The declaration was checked by the broker before this context was created.
/// Workers still recheck restrictions, explicit denials, grants and room rules.
#[derive(Clone)]
pub struct MatrixAuthorization {
    pub subject: String,
    pub capability: String,
    pub origin_room: Option<String>,
    pub consent: Box<PermissionStore>,
    pub flow_context: Option<ContextId>,
    pub flow_epoch: Option<u64>,
}

impl std::fmt::Debug for MatrixAuthorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MatrixAuthorization").field("subject", &self.subject)
            .field("capability", &self.capability).field("origin_room", &self.origin_room).finish()
    }
}

impl MatrixAuthorization {
    pub fn new(subject: &str, capability: &str, origin_room: Option<&str>, store: &PermissionStore) -> Self {
        Self {
            subject: subject.to_string(), capability: capability.to_string(),
            origin_room: origin_room.map(str::to_string), consent: Box::new(store.clone()),
            flow_context: None,
            flow_epoch: None,
        }
    }

    pub fn with_flow(mut self, context: ContextId) -> Self {
        self.flow_epoch = a2app_core::information_flow::context_epoch(&context).ok();
        self.flow_context = Some(context);
        self
    }

    pub fn check_context(&self) -> Result<(), String> {
        let context = self.flow_context.as_ref().ok_or("Missing host information-flow context.")?;
        crate::a2app::information_flow::current_context(context)?;
        a2app_core::information_flow::ensure_context_epoch(context,
            self.flow_epoch.ok_or("Missing information-flow activation.")?)
    }

    pub fn check_flow(&self, room: Option<&str>, access: RoomAccess) -> Result<(), String> {
        let context = self.flow_context.as_ref().ok_or("Missing host information-flow context.")?;
        self.check_context()?;
        let epoch = self.flow_epoch.ok_or("Missing information-flow activation.")?;
        // A read's room/event/search parameters can carry private data too.
        // Recheck their destination after queuing, just like a write body.
        if let Some(room) = room {
            flow::ensure_allowed_for_activation(context, epoch, &Recipient::MatrixRoom {
                account: context.account().into(), room: room.into(),
            })?;
        } else if access == RoomAccess::Write {
            return Err("Missing output room.".into());
        }
        Ok(())
    }

    pub fn commit_action(&self, target: &str, payload: &serde_json::Value) -> Result<(), String> {
        self.check_context()?;
        let context = self.flow_context.as_ref().ok_or("Missing host information-flow context.")?;
        flow::commit_exact_action_for_activation(context, self.flow_epoch.ok_or("Missing information-flow activation.")?,
            &SensitiveAction { kind: self.capability.clone(), target: target.into() }, payload)
    }

    pub fn permits(&self, store: &PermissionStore, room: Option<&str>) -> bool {
        let Some(cap) = a2app_core::capabilities::by_id(&self.capability) else { return false };
        let context = PermissionContext { origin_room: self.origin_room.as_deref(), target_room: room };
        let decision = |store: &PermissionStore| store.effective_capability_for_in_context(
            &self.subject, |_| true, |_| true, cap, context,
        );
        decision(&self.consent) == Effective::Granted
            && match decision(store) {
                Effective::Granted => true,
                Effective::NeedsPrompt => self.consent.has_request_once(&self.subject, cap.id, context),
                Effective::Denied | Effective::Undeclared => false,
            }
    }
}

tokio::task_local! {
    static AUTHORIZATION: MatrixAuthorization;
    static FLOW_ACTIVATION: (ContextId, u64);
}

pub async fn with_authorization<T>(authorization: MatrixAuthorization, future: impl std::future::Future<Output = T>) -> T {
    AUTHORIZATION.scope(authorization, future).await
}

pub async fn with_flow_activation<T>(context: ContextId, epoch: u64, future: impl std::future::Future<Output = T>) -> T {
    FLOW_ACTIVATION.scope((context, epoch), future).await
}

fn operation_context() -> Option<ContextId> {
    AUTHORIZATION.try_with(|authorization| authorization.flow_context.clone()).ok().flatten()
        .or_else(|| FLOW_ACTIVATION.try_with(|(context, _)| context.clone()).ok())
}

/// Record the actual SDK operation after its final authorization check.
///
/// A failed or cancelled request may already have transmitted data.
pub async fn audit_flow_operation<T, E>(context: &ContextId, recipient: Option<Recipient>, operation: impl std::future::IntoFuture<Output = Result<T, E>>) -> Result<T, E> {
    let attempt = a2app_core::protection_audit::Attempt::start(context, recipient, a2app_core::protection_audit::ActivityKind::MatrixOperation);
    let result = operation.into_future().await;
    attempt.finish(result.is_ok());
    result
}

pub async fn audit_room_operation<T, E>(room: &str, operation: impl std::future::IntoFuture<Output = Result<T, E>>) -> Result<T, E> {
    if let Some(context) = operation_context() {
        let recipient = Recipient::MatrixRoom { account: context.account().into(), room: room.into() };
        audit_flow_operation(&context, Some(recipient), operation).await
    } else { operation.into_future().await }
}

pub async fn audit_server_operation<T, E>(url: &str, operation: impl std::future::IntoFuture<Output = Result<T, E>>) -> Result<T, E> {
    if let Some(context) = operation_context() {
        audit_flow_operation(&context, Recipient::network_origin(url).ok(), operation).await
    } else { operation.into_future().await }
}

pub fn ensure_live_activation() -> Result<(), String> {
    FLOW_ACTIVATION.try_with(|(context, epoch)| a2app_core::information_flow::ensure_context_epoch(context, *epoch))
        .unwrap_or(Ok(()))
}

/// Read the identity captured when this worker was queued, never the epoch of
/// whichever instance currently occupies the same durable compartment.
fn captured_flow_epoch(context: &ContextId) -> Result<u64, String> {
    if let Ok(result) = FLOW_ACTIVATION.try_with(|(captured, epoch)| {
        if captured == context { Ok(*epoch) }
        else { Err("Information-flow worker context mismatch.".to_string()) }
    }) { return result; }
    AUTHORIZATION.try_with(|authorization| {
        if authorization.flow_context.as_ref() != Some(context) {
            return Err("Information-flow worker authorization mismatch.".to_string());
        }
        authorization.flow_epoch.ok_or_else(|| "Missing information-flow activation.".to_string())
    }).map_err(|_| "Missing information-flow worker activation.".to_string())?
}

/// Atomically check this worker's captured activation and current sharing rule.
pub fn ensure_flow_output(context: &ContextId, recipient: &Recipient) -> Result<(), String> {
    crate::a2app::information_flow::current_context(context)?;
    flow::ensure_allowed_for_activation(context, captured_flow_epoch(context)?, recipient)
}

pub fn ensure_room_flow_output(context: &ContextId, room: &str) -> Result<(), String> {
    ensure_flow_output(context, &Recipient::MatrixRoom { account: context.account().into(), room: room.into() })
}

/// A newly granted authority cannot authorize an older worker activation.
pub fn commit_flow_action(context: &ContextId, action: &SensitiveAction, payload: &serde_json::Value) -> Result<(), String> {
    crate::a2app::information_flow::current_context(context)?;
    flow::commit_exact_action_for_activation(context, captured_flow_epoch(context)?, action, payload)
}

/// Matrix query parameters are plaintext output to the homeserver even when
/// their target room is encrypted. Require sharing with that exact origin.
pub fn ensure_server_output(url: &str) -> Result<(), String> {
    ensure_live_activation()?;
    AUTHORIZATION.try_with(|authorization| {
        let context = authorization.flow_context.as_ref().ok_or("Missing host information-flow context.")?;
        authorization.check_context()?;
        let recipient = Recipient::network_origin(url)?;
        flow::ensure_allowed_for_activation(context,
            authorization.flow_epoch.ok_or("Missing information-flow activation.")?, &recipient)
    }).map_err(|_| "Missing host information-flow authorization.".to_string())?
}

pub fn room_access_allowed(room: &str, access: RoomAccess) -> bool {
    with_current_policy(|store| {
        store.room_policy(Some(room), access) != PolicyDecision::Deny
            && AUTHORIZATION.try_with(|auth| auth.permits(store, Some(room))
                && auth.check_flow(Some(room), access).is_ok()).unwrap_or(true)
    })
}

/// The worker must still require source sharing consent for the actual URL.
pub fn network_allowed(subject: &str, origin_room: Option<&str>, url: &str, consent: &PermissionStore) -> bool {
    with_current_policy(|store| network_permitted(store, consent, subject, origin_room, url))
}

fn network_permitted(store: &PermissionStore, consent: &PermissionStore, subject: &str, origin_room: Option<&str>, url: &str) -> bool {
    use a2app_core::permissions::{GrantState, Permission};
    let context = PermissionContext { origin_room, target_room: origin_room };
    !store.is_restricted(subject)
        && store.state(subject, Permission::Network) != GrantState::Denied
        && store.capability_state(subject, "network.http") != GrantState::Denied
        && consent.is_url_allowed(subject, url, context)
        && (store.is_url_allowed(subject, url, context)
            || consent.has_network_request_once(subject, url, context))
}

pub fn ensure_room_access(room: &str, access: RoomAccess) -> Result<(), String> {
    ensure_live_activation()?;
    with_current_policy(|store| {
        if store.room_policy(Some(room), access) == PolicyDecision::Deny {
            return Err(ROOM_ACCESS_DENIED.to_string());
        }
        AUTHORIZATION.try_with(|auth| {
            if !auth.permits(store, Some(room)) { return Err(ROOM_ACCESS_DENIED.to_string()); }
            auth.check_flow(Some(room), access)
        }).unwrap_or(Ok(()))
    })
}

/// Recheck a host-defined sensitive target immediately before its effect.
pub fn commit_sensitive_target(target: &str, payload: &serde_json::Value) -> Result<(), String> {
    ensure_live_activation()?;
    AUTHORIZATION.try_with(|auth| {
        let context = auth.flow_context.as_ref().ok_or("Missing host information-flow context.")?;
        auth.check_context()?;
        flow::commit_exact_action_for_activation(context,
            auth.flow_epoch.ok_or("Missing information-flow activation.")?, &SensitiveAction {
            kind: auth.capability.clone(), target: target.into(),
        }, payload)
    }).map_err(|_| "Missing host information-flow authorization.".to_string())?
}

pub fn global_room_access_allowed(room: &str, access: RoomAccess) -> bool {
    with_current_policy(|store| store.room_policy(Some(room), access) != PolicyDecision::Deny)
}

/// Filter every returned room identifier, including a resolved alias or a
/// successor/DM that was unknown before the SDK call. This also runs on the UI
/// thread, so a rule changed while a network request was in flight wins.
pub fn filter_read_result(
    result: &str,
    store: &PermissionStore,
    authorization: Option<&MatrixAuthorization>,
) -> Result<String, String> {
    let mut value: serde_json::Value = serde_json::from_str(result).map_err(|_| "invalid Matrix service response")?;
    let allowed = |room: &str| {
        store.room_policy(Some(room), RoomAccess::Read) != PolicyDecision::Deny
            && authorization.is_none_or(|auth| auth.permits(store, Some(room)))
    };
    let row_allowed = |row: &serde_json::Value| {
        row.get("room_id").or_else(|| row.get("space_id"))
            .and_then(serde_json::Value::as_str).is_none_or(&allowed)
    };
    if !row_allowed(&value) {
        return Err(ROOM_ACCESS_DENIED.to_string());
    }
    for key in ["rooms", "spaces", "invites", "results"] {
        if let Some(rows) = value.get_mut(key).and_then(serde_json::Value::as_array_mut) {
            rows.retain(&row_allowed);
        }
    }
    // A stale aggregate can reveal activity in a room removed by the filter.
    if let Some(object) = value.as_object_mut() {
        object.remove("searched_rooms");
    }
    Ok(value.to_string())
}

pub fn filter_current_read_result(result: &str) -> Result<String, String> {
    let authorization = AUTHORIZATION.try_with(Clone::clone).ok();
    with_current_policy(|store| filter_read_result(result, store, authorization.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2app_core::permissions::{GrantDuration, GrantState, Permission, RoomScope};

    #[test]
    fn response_filter_keeps_non_room_payloads_intact() {
        let store = PermissionStore::default();
        let response = filter_read_result(r#"{"messages":[{"body":"hello"}],"count":1}"#, &store, None).unwrap();
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["messages"][0]["body"], "hello");
        assert_eq!(value["count"], 1);
    }

    #[test]
    fn response_filter_hides_blocked_rooms_spaces_invites_and_search_hits() {
        let mut store = PermissionStore::default();
        store.set_room_policy("!private:s", RoomAccess::Read, PolicyDecision::Deny);
        store.set_space_policy("!secret:s", RoomAccess::Read, PolicyDecision::Deny);
        store.set_room_spaces("!public:s", vec![]);
        store.set_room_spaces("!private:s", vec![]);
        store.set_room_spaces("!secret:s", vec![]);
        store.set_room_spaces("!nested:s", vec!["!secret:s".into()]);
        for key in ["rooms", "spaces", "invites", "results"] {
            let id = if key == "spaces" { "space_id" } else { "room_id" };
            let input = serde_json::json!({ key: [
                { id: "!public:s", "name": "Public" },
                { id: "!private:s", "name": "Private" },
                { id: "!secret:s", "name": "Secret" },
                { id: "!nested:s", "name": "Nested" },
                { id: "!unknown:s", "name": "Unresolved membership" },
            ], "searched_rooms": 5 });
            let filtered = filter_read_result(&input.to_string(), &store, None).unwrap();
            let output: serde_json::Value = serde_json::from_str(&filtered).unwrap();
            assert_eq!(output[key].as_array().unwrap().len(), 1, "{key}");
            assert_eq!(output[key][0][id], "!public:s");
            assert!(output.get("searched_rooms").is_none());
        }
        assert!(filter_read_result(r#"{"room_id":"!private:s","name":"Private"}"#, &store, None).is_err());
    }

    #[test]
    fn collection_consent_checks_each_target_and_hard_denials_again() {
        let mut store = PermissionStore::default();
        store.grant_scoped("app", Permission::MatrixRoomsList, Some("matrix.rooms.list"),
            RoomScope::room("!allowed:s"), GrantDuration::Always, None).unwrap();
        let auth = MatrixAuthorization::new("app", "matrix.rooms.list", None, &store);
        assert!(auth.permits(&store, Some("!allowed:s")));
        assert!(!auth.permits(&store, Some("!elsewhere:s")));
        let response = r#"{"rooms":[{"room_id":"!allowed:s"},{"room_id":"!elsewhere:s"}]}"#;
        let output: serde_json::Value = serde_json::from_str(&filter_read_result(response, &store, Some(&auth)).unwrap()).unwrap();
        assert_eq!(output["rooms"].as_array().unwrap().len(), 1);
        store.set_room_policy("!allowed:s", RoomAccess::Read, PolicyDecision::Deny);
        assert!(!auth.permits(&store, Some("!allowed:s")));
        store.set_room_policy("!allowed:s", RoomAccess::Read, PolicyDecision::Allow);
        store.set("app", Permission::MatrixRoomsList, GrantState::Denied);
        assert!(!auth.permits(&store, Some("!allowed:s")));
    }

    #[test]
    fn revocation_cancels_pending_reads_but_a_consumed_once_receipt_remains_valid() {
        let mut store = PermissionStore::default();
        let grant = store.grant_scoped("app", Permission::MatrixRoomsRead, Some("matrix.rooms.messages.read"),
            RoomScope::room("!allowed:s"), GrantDuration::Always, None).unwrap();
        let durable = MatrixAuthorization::new("app", "matrix.rooms.messages.read", Some("!origin:s"), &store);
        store.remove_scoped_grant(grant);
        assert!(!durable.permits(&store, Some("!allowed:s")));
        let once = store.grant_scoped("app", Permission::MatrixRoomsRead, Some("matrix.rooms.messages.read"),
            RoomScope::room("!allowed:s"), GrantDuration::RobrixSession, None).unwrap();
        store.mark_request_once(once);
        let receipt = MatrixAuthorization::new("app", "matrix.rooms.messages.read", Some("!origin:s"), &store);
        store.remove_scoped_grant(once);
        assert!(receipt.permits(&store, Some("!allowed:s")));
        assert!(!receipt.permits(&store, Some("!elsewhere:s")));
        store.set_room_policy("!allowed:s", RoomAccess::Read, PolicyDecision::Deny);
        assert!(!receipt.permits(&store, Some("!allowed:s")));
    }

    #[test]
    fn network_worker_checks_current_url_consent_and_explicit_revocation() {
        use a2app_core::permissions::NetworkScope;
        let mut store = PermissionStore::default();
        let grant = store.allow_network("app", NetworkScope::Origin("https://example.com".into()),
            RoomScope::room("!room:s"), GrantDuration::Always, None).unwrap();
        let consent = store.clone();
        assert!(network_permitted(&store, &consent, "app", Some("!room:s"), "https://example.com/path"));
        assert!(!network_permitted(&store, &consent, "app", Some("!room:s"), "https://other.example/path"));
        assert!(!network_permitted(&store, &consent, "app", Some("!other:s"), "https://example.com/path"));
        store.remove_network_grant(grant);
        assert!(!network_permitted(&store, &consent, "app", Some("!room:s"), "https://example.com/path"));
        let once = store.allow_network("app", NetworkScope::ExactUrl("https://example.com/path".into()),
            RoomScope::room("!room:s"), GrantDuration::RobrixSession, None).unwrap();
        store.mark_network_request_once(once);
        let consent = store.clone();
        store.remove_network_grant(once);
        assert!(network_permitted(&store, &consent, "app", Some("!room:s"), "https://example.com/path"));
        assert!(!network_permitted(&store, &consent, "app", Some("!room:s"), "https://example.com/other"));
        store.set("app", Permission::Network, GrantState::Denied);
        assert!(!network_permitted(&store, &consent, "app", Some("!room:s"), "https://example.com/path"));
    }

    #[test]
    fn broad_network_group_grants_never_replace_url_consent() {
        let mut store = PermissionStore::default();
        store.set("app", Permission::Network, GrantState::Granted);
        assert!(!network_permitted(&store, &store, "app", None, "https://example.com/path"));
    }

    #[test]
    fn homeserver_output_needs_host_flow_authorization() {
        assert!(ensure_server_output("https://matrix.example").is_err());
        let authorization = MatrixAuthorization::new("app", "matrix.room.event.read", Some("!room:s"), &PermissionStore::default());
        assert!(authorization.check_flow(Some("!room:s"), RoomAccess::Read).is_err());
    }

    #[tokio::test]
    async fn queued_worker_keeps_old_epoch_after_new_instance_gets_authority() {
        let root = std::env::temp_dir().join(format!("robrix-matrix-epoch-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let mut registry = flow::Registry::open(&root).unwrap();
        let context = ContextId::Agent { account: "alice".into(), room: "!room:s".into() };
        let action = SensitiveAction { kind: "matrix.rooms.message.send".into(), target: "!room:s".into() };
        registry.register_context(&context).unwrap();
        registry.add_influences(&context, [flow::Influence::InternetOrigin("https://example.com".into())]).unwrap();
        let queued_epoch = registry.context_epoch(&context).unwrap();
        registry.remove_context(&context);
        registry.register_context(&context).unwrap();
        registry.grant_authority(&context, action.clone(), flow::AuthoritySession::RobrixSession).unwrap();
        assert!(registry.ensure_action_allowed(&context, &action).is_ok());

        with_flow_activation(context.clone(), queued_epoch, async {
            let captured = captured_flow_epoch(&context).unwrap();
            assert_eq!(captured, queued_epoch);
            assert!(registry.ensure_action_allowed_for_activation(&context, captured, &action).is_err());
            let other = ContextId::Agent { account: "bob".into(), room: "!room:s".into() };
            assert!(captured_flow_epoch(&other).is_err());
        }).await;
        let mut authorization = MatrixAuthorization::new("app", &action.kind, Some("!room:s"), &PermissionStore::default());
        authorization.flow_context = Some(context.clone());
        authorization.flow_epoch = Some(queued_epoch);
        with_authorization(authorization, async {
            let captured = captured_flow_epoch(&context).unwrap();
            assert!(registry.ensure_action_allowed_for_activation(&context, captured, &action).is_err());
        }).await;
        assert!(captured_flow_epoch(&context).is_err());
        assert!(ensure_live_activation().is_ok(), "native host work has no app activation");
        std::fs::remove_dir_all(root).unwrap();
    }
}
