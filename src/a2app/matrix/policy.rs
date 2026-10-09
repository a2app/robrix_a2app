//! Recheck room safety at the worker boundary and before delivering results.

use std::sync::{LazyLock, RwLock};

use a2app_core::permissions::{Effective, PermissionContext, PermissionStore, PolicyDecision, RoomAccess, RoomScope};
use a2app_core::information_flow::{self as flow, ContextId, Recipient, SensitiveAction};

pub const ROOM_ACCESS_DENIED: &str = "room access is blocked by the safety rules in Mini Apps";

static POLICY: LazyLock<RwLock<PermissionStore>> = LazyLock::new(Default::default);
static POLICY_REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
// Effect checks hold the IFC registry lock. Keep their ancestry lookup separate
// from POLICY, whose worker checks can enter the IFC registry themselves.
static EFFECT_SPACE_MEMBERSHIPS: LazyLock<RwLock<(u64, std::collections::BTreeMap<String, std::collections::BTreeSet<String>>)>> = LazyLock::new(Default::default);
static EFFECT_SPACE_ROOTS: LazyLock<RwLock<std::collections::BTreeSet<String>>> = LazyLock::new(Default::default);

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
    *EFFECT_SPACE_MEMBERSHIPS.write().unwrap() = (current, policy.room_space_memberships().clone());
    POLICY_REVISION.store(current, std::sync::atomic::Ordering::SeqCst);
}

pub fn invalidate_room_spaces() {
    POLICY.write().unwrap().clear_room_spaces();
    EFFECT_SPACE_MEMBERSHIPS.write().unwrap().1.clear();
}

pub fn configured_space_ids() -> std::collections::BTreeSet<String> {
    let mut spaces = POLICY.read().unwrap().configured_space_ids();
    spaces.extend(EFFECT_SPACE_ROOTS.read().unwrap().iter().cloned());
    spaces
}

pub fn publish_effect_space_roots(spaces: std::collections::BTreeSet<String>) {
    *EFFECT_SPACE_ROOTS.write().unwrap() = spaces;
}

/// Match a selected space against current host ancestry, never app metadata.
pub fn effect_room_scope_matches(account: &str, scope: &RoomScope, room: &str) -> bool {
    if crate::a2app::information_flow::account().as_deref() != Ok(account) { return false; }
    match scope {
        RoomScope::AllRooms => true,
        RoomScope::Selection { rooms, spaces } => {
            if rooms.iter().any(|id| id == room) || spaces.iter().any(|space| space == room) { return true; }
            let ancestry = EFFECT_SPACE_MEMBERSHIPS.read().unwrap();
            ancestry.0 == super::spaces::policy_spaces_revision()
                && ancestry.1.get(room).is_some_and(|parents| spaces.iter().any(|space| parents.contains(space)))
        }
    }
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
    pub target_room: Option<String>,
    pub consent: Box<PermissionStore>,
    pub flow_context: Option<ContextId>,
    pub flow_epoch: Option<u64>,
    pub flow_payload: Option<serde_json::Value>,
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
            target_room: origin_room.map(str::to_string),
            flow_context: None,
            flow_epoch: None,
            flow_payload: None,
        }
    }

    pub fn with_flow(mut self, context: ContextId) -> Self {
        self.flow_epoch = a2app_core::information_flow::context_epoch(&context).ok();
        self.flow_context = Some(context);
        self
    }

    pub fn with_payload(mut self, payload: serde_json::Value) -> Self {
        self.flow_payload = Some(payload);
        self
    }

    pub fn check_context(&self) -> Result<(), String> {
        let context = self.flow_context.as_ref().ok_or("Missing host information-flow context.")?;
        crate::a2app::information_flow::current_context(context)?;
        a2app_core::information_flow::ensure_context_epoch(context,
            self.flow_epoch.ok_or("Missing information-flow activation.")?)
    }

    pub fn check_current_permission(&self) -> Result<(), String> {
        self.check_context()?;
        if with_current_policy(|store| self.permits_request(store)) { Ok(()) }
        else { Err(ROOM_ACCESS_DENIED.into()) }
    }

    pub fn permits_request(&self, store: &PermissionStore) -> bool {
        let Some(cap) = a2app_core::capabilities::by_id(&self.capability) else { return false };
        if !a2app_core::services::is_room_collection(cap) { return self.permits(store, self.target_room.as_deref()); }
        let context = PermissionContext { origin_room: self.origin_room.as_deref(), target_room: self.target_room.as_deref() };
        // Collection consent starts a filtered query; it does not grant its
        // root or every returned room. Keep the broker's space-root deny gate.
        let decision = |store: &PermissionStore| {
            if cap.id == "matrix.space.rooms.list" && store.capability_room_policy(cap, context) == PolicyDecision::Deny {
                Effective::Denied
            } else {
                store.effective_collection_capability_for_in_context(&self.subject, |_| true, |_| true, cap, context)
            }
        };
        decision(&self.consent) == Effective::Granted
            && match decision(store) {
                Effective::Granted => true,
                Effective::NeedsPrompt => self.has_collection_once(context),
                Effective::Denied | Effective::Undeclared => false,
            }
    }

    fn has_collection_once(&self, context: PermissionContext<'_>) -> bool {
        self.consent.scoped_grants(&self.subject).iter().any(|grant| match &grant.scope {
            a2app_core::permissions::RoomScope::AllRooms => self.consent.has_request_once(&self.subject, &self.capability, context),
            a2app_core::permissions::RoomScope::Selection { rooms, spaces } => rooms.iter().chain(spaces).any(|room|
                self.consent.has_request_once(&self.subject, &self.capability,
                    PermissionContext { origin_room: context.origin_room, target_room: Some(room) })),
        })
    }

    pub fn check_flow(&self, room: Option<&str>, access: RoomAccess) -> Result<(), String> {
        let context = self.flow_context.as_ref().ok_or("Missing host information-flow context.")?;
        self.check_context()?;
        let epoch = self.flow_epoch.ok_or("Missing information-flow activation.")?;
        // Reading Robrix's cache does not send data to a room. Remote query
        // parameters are checked against the homeserver at their SDK boundary.
        if access == RoomAccess::Read { return Ok(()); }
        let final_write = access == RoomAccess::Write && a2app_core::capabilities::by_id(&self.capability)
            .and_then(|capability| capability.flow_contract()).is_some_and(|contract| contract.privileged_effect);
        if let Some(room) = room.filter(|_| !final_write) {
            flow::ensure_allowed_for_activation(context, epoch, &Recipient::MatrixRoom {
                account: context.account().into(), room: room.into(),
            })?;
        } else if room.is_none() && access == RoomAccess::Write {
            return Err("Missing output room.".into());
        }
        Ok(())
    }

    pub fn commit_action(&self, target: &str, payload: &serde_json::Value) -> Result<(), String> {
        self.check_context()?;
        let context = self.flow_context.as_ref().ok_or("Missing host information-flow context.")?;
        let epoch = self.flow_epoch.ok_or("Missing information-flow activation.")?;
        let action = SensitiveAction { kind: self.capability.clone(), target: target.into() };
        // Agents use their exact-action queue; mini-app bridge calls use the
        // effect-review queue. Both approvals are for local staging only.
        if matches!(context, ContextId::Agent { .. }) {
            flow::commit_exact_action_for_activation(context, epoch, &action, payload)
        } else {
            flow::commit_effect_for_activation(context, epoch, None, Some(&action), payload)
        }
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

/// Fixed host pagination still sends the current context's inputs to the
/// homeserver. Check its captured activation and live sharing before polling
/// the SDK future, while keeping caller-selected query review separate.
pub(super) async fn read_server_operation<T, E: std::fmt::Display>(url: &str, query: Option<&ServerQueryApproval>, operation: impl std::future::IntoFuture<Output = Result<T, E>>) -> Result<T, String> {
    ensure_live_activation()?;
    AUTHORIZATION.try_with(|authorization| authorization.check_current_permission()).unwrap_or(Ok(()))?;
    let context = operation_context().ok_or("Missing host information-flow authorization.")?;
    let recipient = Recipient::network_origin(url)?;
    if let Some(query) = query {
        if query.recipient != recipient { return Err("The reviewed query belongs to a different homeserver.".into()); }
        let same_worker = AUTHORIZATION.try_with(|authorization|
            authorization.subject == query.authorization.subject
                && authorization.capability == query.authorization.capability
                && authorization.target_room == query.authorization.target_room
                && authorization.flow_context == query.authorization.flow_context
                && authorization.flow_epoch == query.authorization.flow_epoch
                && authorization.flow_payload == query.authorization.flow_payload).unwrap_or(false);
        if !same_worker { return Err("The reviewed query belongs to a different worker request.".into()); }
        query.check()?;
    } else { ensure_flow_output(&context, &recipient)?; }
    audit_server_operation(url, operation).await.map_err(|error| error.to_string())
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
pub async fn ensure_server_output(url: &str) -> Result<(), String> {
    let payload = AUTHORIZATION.try_with(|authorization| authorization.flow_payload.clone()).ok().flatten()
        .unwrap_or(serde_json::Value::Null);
    ensure_server_output_with_payload(url, &payload).await
}

/// The reviewed search uses the same deterministic room filter as the SDK.
pub fn search_server_parameters(query: &str, room_ids: &[matrix_sdk::ruma::OwnedRoomId], limit: u32) -> serde_json::Value {
    let mut room_ids = room_ids.to_vec();
    room_ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    serde_json::json!({ "query": query, "room_ids": room_ids,
        "result_limit": limit, "keys": ["content.body"], "order_by": "recent" })
}

/// Record each authorized room before its cached messages reach this worker.
pub fn record_read_rooms(rooms: &[matrix_sdk::ruma::OwnedRoomId]) -> Result<(), String> {
    ensure_live_activation()?;
    let authorization = AUTHORIZATION.try_with(Clone::clone)
        .map_err(|_| "Missing host information-flow authorization.".to_string())?;
    authorization.check_current_permission()?;
    let context = authorization.flow_context.as_ref().ok_or("Missing host information-flow context.")?;
    let epoch = authorization.flow_epoch.ok_or("Missing information-flow activation.")?;
    for room in rooms { ensure_room_access(room.as_str(), RoomAccess::Read)?; }
    flow::add_sources_for_activation(context, epoch, rooms.iter().map(|room|
        flow::Source::Room { account: context.account().into(), room: room.to_string() }))?;
    flow::add_influences_for_activation(context, epoch, rooms.iter().map(|room|
        flow::Influence::RoomContent { account: context.account().into(), room: room.to_string() }))
}

/// Review and consume one immutable query before it reaches the homeserver.
pub async fn ensure_server_output_with_payload(url: &str, parameters: &serde_json::Value) -> Result<(), String> {
    approve_server_query(url, parameters).await.map(|_| ())
}

/// Approval for the pages of one bounded, immutable host query.
///
/// A one-time choice covers this service call, including its SDK pagination.
/// Each page still checks current capability consent, activation and inputs.
pub(super) struct ServerQueryApproval {
    authorization: MatrixAuthorization,
    recipient: Recipient,
    sources: flow::Label,
    influences: flow::Influences,
}

impl ServerQueryApproval {
    pub(super) fn check(&self) -> Result<(), String> {
        self.authorization.check_current_permission()?;
        let context = self.authorization.flow_context.as_ref().ok_or("Missing host information-flow context.")?;
        if flow::labels(context)? != self.sources || flow::influences(context)? != self.influences {
            return Err("This app received new data while loading. Load this view again to review its permission.".into());
        }
        Ok(())
    }
}

pub(super) async fn begin_server_query(url: &str) -> Result<ServerQueryApproval, String> {
    let parameters = AUTHORIZATION.try_with(|authorization| authorization.flow_payload.clone()).ok().flatten()
        .ok_or("Missing captured Matrix query.")?;
    approve_server_query(url, &parameters).await
}

async fn approve_server_query(url: &str, parameters: &serde_json::Value) -> Result<ServerQueryApproval, String> {
    ensure_live_activation()?;
    let authorization = AUTHORIZATION.try_with(Clone::clone)
        .map_err(|_| "Missing host information-flow authorization.".to_string())?;
    let context = authorization.flow_context.as_ref().ok_or("Missing host information-flow context.")?;
    authorization.check_current_permission()?;
    let epoch = authorization.flow_epoch.ok_or("Missing information-flow activation.")?;
    let recipient = Recipient::network_origin(url)?;
    let payload = serde_json::json!({ "destination": url, "operation": authorization.capability, "parameters": parameters });
    let review = flow::prepare_effect_for_activation(context, epoch, Some(&recipient), None, &payload)?;
    crate::a2app::effect_review::request(review, true).await?;
    authorization.check_current_permission()?;
    let capture = flow::prepare_effect_for_activation(context, epoch, Some(&recipient), None, &payload)?;
    flow::commit_effect_for_activation(context, epoch, Some(&recipient), None, &payload)?;
    Ok(ServerQueryApproval { authorization, recipient, sources: capture.sources, influences: capture.influences })
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
pub async fn commit_sensitive_target(target: &str, payload: &serde_json::Value) -> Result<(), String> {
    ensure_live_activation()?;
    let auth = AUTHORIZATION.try_with(Clone::clone)
        .map_err(|_| "Missing host information-flow authorization.".to_string())?;
    let context = auth.flow_context.as_ref().ok_or("Missing host information-flow context.")?;
    auth.check_current_permission()?;
    let epoch = auth.flow_epoch.ok_or("Missing information-flow activation.")?;
    let action = SensitiveAction { kind: auth.capability.clone(), target: target.into() };
    let contract = a2app_core::capabilities::by_id(&auth.capability)
        .and_then(|capability| capability.flow_contract()).ok_or("Missing information-flow contract.")?;
    let homeserver = crate::sliding_sync::get_client().map(|client| client.homeserver().to_string());
    let recipient = contract.recipient(context.account(), Some(target), payload, homeserver.as_deref())?;
    let review = flow::prepare_effect_for_activation(context, epoch, recipient.as_ref(), Some(&action), payload)?;
    crate::a2app::effect_review::request(review, true).await?;
    auth.check_current_permission()?;
    if contract.output == a2app_core::capabilities::FlowOutput::TargetRoom {
        ensure_room_access(target, RoomAccess::Write)?;
    }
    flow::commit_effect_for_activation(context, epoch, recipient.as_ref(), Some(&action), payload)
}

/// Recheck room permissions before reading cached data or changing a room.
///
/// Reads release no data to room members. Any remote query is reviewed for its
/// actual server destination immediately before the SDK sends it.
pub async fn review_room_access(room: &str, access: RoomAccess) -> Result<(), String> {
    ensure_room_access(room, access)
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
    // An upgrade pointer is data from the already-authorized source room.
    // It grants no read access to the successor; blocked rooms stay hidden.
    let successor_reference = authorization.is_some_and(|auth|
        auth.capability == "matrix.room.successor.read" && auth.permits(store, auth.target_room.as_deref()))
        && value["upgraded"].as_bool() == Some(true)
        && value["room_id"].as_str().is_some_and(|room| store.room_policy(Some(room), RoomAccess::Read) != PolicyDecision::Deny);
    if !row_allowed(&value) && !successor_reference {
        return Err(ROOM_ACCESS_DENIED.to_string());
    }
    if successor_reference && value["room_id"].as_str().is_some_and(|room| !allowed(room)) {
        value["name"] = serde_json::Value::Null;
    }
    for key in ["rooms", "spaces", "invites", "results"] {
        if let Some(rows) = value.get_mut(key).and_then(serde_json::Value::as_array_mut) {
            rows.retain(&row_allowed);
        }
    }
    // Keep the legacy Search response field, but count only rooms represented
    // in the authorized results. The raw aggregate can reveal filtered rooms;
    // removing the field breaks saved apps that still display it.
    let result_room_count = value.get("results").and_then(serde_json::Value::as_array)
        .map(|rows| rows.iter().filter_map(|row| row.get("room_id")
            .and_then(serde_json::Value::as_str)).collect::<std::collections::BTreeSet<_>>().len());
    if let Some(object) = value.as_object_mut() {
        if let Some(count) = result_room_count {
            object.insert("searched_rooms".into(), count.into());
        } else {
            object.remove("searched_rooms");
        }
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
            if key == "results" {
                assert_eq!(output["searched_rooms"], 1);
            } else {
                assert!(output.get("searched_rooms").is_none());
            }
        }
        assert!(filter_read_result(r#"{"room_id":"!private:s","name":"Private"}"#, &store, None).is_err());
    }

    #[test]
    fn search_count_uses_unique_visible_rooms_even_without_the_raw_aggregate() {
        let mut store = PermissionStore::default();
        store.set_room_policy("!private:s", RoomAccess::Read, PolicyDecision::Deny);
        store.set_room_spaces("!public:s", vec![]);
        store.set_room_spaces("!private:s", vec![]);
        for raw_count in [None, Some(99)] {
            let mut input = serde_json::json!({ "results": [
                { "room_id": "!public:s", "event_id": "$first:s" },
                { "room_id": "!public:s", "event_id": "$second:s" },
                { "room_id": "!private:s", "event_id": "$hidden:s" },
            ] });
            if let Some(count) = raw_count { input["searched_rooms"] = count.into(); }
            let filtered = filter_read_result(&input.to_string(), &store, None).unwrap();
            let output: serde_json::Value = serde_json::from_str(&filtered).unwrap();
            assert_eq!(output["results"].as_array().unwrap().len(), 2);
            assert_eq!(output["searched_rooms"], 1);
        }
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
    fn collection_worker_keeps_child_subset_consent_without_authorizing_its_root() {
        for capability in ["matrix.space.rooms.list", "matrix.rooms.messages.search"] {
            let cap = a2app_core::capabilities::by_id(capability).unwrap();
            let mut store = PermissionStore::default();
            store.set_global_policy(RoomAccess::Read, PolicyDecision::Ask);
            let grant = store.grant_scoped("app", cap.group.unwrap(), Some(capability),
                RoomScope::room("!child:s"), GrantDuration::RobrixSession, None).unwrap();
            let mut auth = MatrixAuthorization::new("app", capability, Some("!origin:s"), &store);
            let root = if capability == "matrix.space.rooms.list" { "!space:s" } else { "!origin:s" };
            auth.target_room = Some(root.into());
            assert!(auth.permits_request(&store), "{capability}");
            assert!(!auth.permits(&store, Some(root)));
            assert!(auth.permits(&store, Some("!child:s")));
            assert!(!auth.permits(&store, Some("!sibling:s")));
            let response = r#"{"rooms":[{"room_id":"!child:s"},{"room_id":"!sibling:s"}]}"#;
            let filtered: serde_json::Value = serde_json::from_str(&filter_read_result(response, &store, Some(&auth)).unwrap()).unwrap();
            assert_eq!(filtered["rooms"], serde_json::json!([{ "room_id": "!child:s" }]));

            let mut denied_child = store.clone();
            denied_child.set_room_policy("!child:s", RoomAccess::Read, PolicyDecision::Deny);
            assert!(!auth.permits(&denied_child, Some("!child:s")));
            let filtered: serde_json::Value = serde_json::from_str(&filter_read_result(response, &denied_child, Some(&auth)).unwrap()).unwrap();
            assert!(filtered["rooms"].as_array().unwrap().is_empty());

            let mut revoked = store.clone();
            revoked.remove_scoped_grant(grant);
            assert!(!auth.permits_request(&revoked));
            let once = revoked.grant_scoped("app", cap.group.unwrap(), Some(capability),
                RoomScope::room("!child:s"), GrantDuration::RobrixSession, None).unwrap();
            revoked.mark_request_once(once);
            let mut once_auth = MatrixAuthorization::new("app", capability, Some("!origin:s"), &revoked);
            once_auth.target_room = Some(root.into());
            revoked.remove_scoped_grant(once);
            assert!(once_auth.permits_request(&revoked), "collection once proof belongs to its selected child, not its root");
            assert!(once_auth.permits(&revoked, Some("!child:s")));
            assert!(!once_auth.permits(&revoked, Some("!sibling:s")));
            revoked.set_capability("app", capability, GrantState::Denied);
            assert!(!once_auth.permits_request(&revoked));
            let mut denied = store.clone();
            denied.set_capability("app", capability, GrantState::Denied);
            assert!(!auth.permits_request(&denied));
            let mut denied = store.clone();
            denied.set_global_policy(RoomAccess::Read, PolicyDecision::Deny);
            assert!(!auth.permits_request(&denied));
            if capability == "matrix.space.rooms.list" {
                store.set_space_policy("!space:s", RoomAccess::Read, PolicyDecision::Deny);
                assert!(!auth.permits_request(&store));
            }
        }
    }

    #[test]
    fn revocation_cancels_pending_reads_but_a_consumed_once_receipt_remains_valid() {
        let mut store = PermissionStore::default();
        let grant = store.grant_scoped("app", Permission::MatrixRoomsRead, Some("matrix.rooms.messages.read"),
            RoomScope::room("!allowed:s"), GrantDuration::Always, None).unwrap();
        let mut durable = MatrixAuthorization::new("app", "matrix.rooms.messages.read", Some("!origin:s"), &store);
        durable.target_room = Some("!allowed:s".into());
        assert!(durable.permits_request(&store));
        store.remove_scoped_grant(grant);
        assert!(!durable.permits_request(&store));
        assert!(!durable.permits(&store, Some("!allowed:s")));
        let once = store.grant_scoped("app", Permission::MatrixRoomsRead, Some("matrix.rooms.messages.read"),
            RoomScope::room("!allowed:s"), GrantDuration::RobrixSession, None).unwrap();
        store.mark_request_once(once);
        let mut receipt = MatrixAuthorization::new("app", "matrix.rooms.messages.read", Some("!origin:s"), &store);
        receipt.target_room = Some("!allowed:s".into());
        store.remove_scoped_grant(once);
        assert!(receipt.permits_request(&store));
        assert!(receipt.permits(&store, Some("!allowed:s")));
        assert!(!receipt.permits(&store, Some("!elsewhere:s")));
        store.set_room_policy("!allowed:s", RoomAccess::Read, PolicyDecision::Deny);
        assert!(!receipt.permits_request(&store));
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

    #[tokio::test]
    async fn homeserver_output_needs_host_flow_authorization() {
        assert!(ensure_server_output("https://matrix.example").await.is_err());
        let authorization = MatrixAuthorization::new("app", "matrix.room.event.read", Some("!room:s"), &PermissionStore::default());
        assert!(authorization.check_flow(Some("!room:s"), RoomAccess::Read).is_err());
    }

    // Each Tokio test owns a separate runtime/test thread. Keep the global
    // policy guard through authorization awaits so another test cannot replace it.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn remote_read_boundary_checks_live_sharing_and_bound_query_before_polling_sdk() {
        use std::{cell::Cell, future::Future, task::{Context, Poll, Waker}};
        let _review_lock = crate::a2app::effect_review::TEST_LOCK.lock().unwrap();
        let account = format!("@remote-read-boundary-{}:test", std::process::id());
        let previous_account = crate::a2app::information_flow::TEST_ACCOUNT.with(|a| a.replace(Some(account)));
        let context = crate::a2app::information_flow::begin_agent_session("!remote-read-boundary:test").unwrap();
        flow::add_sources(&context, [flow::Source::Room { account: context.account().into(), room: "!private-remote-read:test".into() }]).unwrap();
        let subject = a2app_core::permissions::agent_subject(context.room().unwrap());
        let mut store = PermissionStore::default();
        store.grant_scoped(&subject, Permission::MatrixRoomRead, Some("matrix.room.messages.paginate"),
            RoomScope::room(context.room().unwrap()), GrantDuration::Always, None).unwrap();
        let auth = MatrixAuthorization::new(&subject, "matrix.room.messages.paginate", context.room(), &store)
            .with_flow(context.clone()).with_payload(serde_json::json!({ "before": "$anchor:test", "limit": 20 }));
        publish_permission_policy(&store);
        let url = "https://remote-read-boundary.test";
        let recipient = Recipient::network_origin(url).unwrap();
        let polled = Cell::new(0);
        with_authorization(auth.clone(), async {
            assert!(read_server_operation(url, None, async { polled.set(polled.get() + 1); Ok::<_, &str>(()) }).await.is_err());
            assert_eq!(polled.get(), 0, "missing homeserver sharing must stop before the SDK future");
            let grants: Vec<_> = flow::labels(&context).unwrap().into_iter().map(|source|
                flow::grant_sharing(source, recipient.clone(), flow::ReaderScope::Context(context.clone()), flow::SharingDuration::RobrixSession).unwrap()).collect();
            read_server_operation(url, None, async { polled.set(polled.get() + 1); Ok::<_, &str>(()) }).await.unwrap();
            for grant in grants { flow::revoke_sharing(grant).unwrap(); }
            assert!(read_server_operation(url, None, async { polled.set(polled.get() + 1); Ok::<_, &str>(()) }).await.is_err());
            assert_eq!(polled.get(), 1, "revocation must block the next fixed pagination page");

            // One exact anchor approval covers its bounded top-up pages, while
            // retaining no arbitrary sharing grant for the homeserver.
            let mut approval = Box::pin(begin_server_query(url));
            assert!(approval.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
            let mut pending = crate::a2app::effect_review::take_pending();
            assert_eq!(pending.len(), 1);
            pending[0].approve(flow::approve_effect_once).unwrap();
            pending.pop().unwrap().finish(Ok(()));
            let query = match approval.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
                Poll::Ready(Ok(query)) => query,
                _ => panic!("the approved query must resume"),
            };
            assert!(flow::ensure_allowed(&context, &recipient).is_err());
            for _ in 0..2 {
                read_server_operation(url, Some(&query), async { polled.set(polled.get() + 1); Ok::<_, &str>(()) }).await.unwrap();
            }
            assert_eq!(polled.get(), 3);
            assert!(read_server_operation("https://different-server.test", Some(&query), async { polled.set(polled.get() + 1); Ok::<_, &str>(()) }).await.is_err());
            let changed = auth.clone().with_payload(serde_json::json!({ "before": "$different:test", "limit": 20 }));
            assert!(with_authorization(changed, read_server_operation(url, Some(&query), async { polled.set(polled.get() + 1); Ok::<_, &str>(()) })).await.is_err());
            store.set_capability(&subject, "matrix.room.messages.paginate", GrantState::Denied);
            publish_permission_policy(&store);
            assert!(read_server_operation(url, Some(&query), async { polled.set(polled.get() + 1); Ok::<_, &str>(()) }).await.is_err());
            assert_eq!(polled.get(), 3, "a mismatched or revoked query must not poll the SDK");
        }).await;
        store.set_capability(&subject, "matrix.room.messages.paginate", GrantState::Granted);
        publish_permission_policy(&store);
        flow::remove_context(&context).unwrap();
        assert!(with_authorization(auth, read_server_operation(url, None, async { polled.set(polled.get() + 1); Ok::<_, &str>(()) })).await.is_err());
        assert_eq!(polled.get(), 3, "a retired activation must not poll the SDK");
        publish_permission_policy(&PermissionStore::default());
        crate::a2app::information_flow::TEST_ACCOUNT.with(|a| { a.replace(previous_account); });
    }

    #[tokio::test]
    async fn saved_edited_search_reads_locally_after_one_ordinary_approval() {
        use makepad_widgets::*;
        use a2app_core::{manifest::AppRegistry, services::{self, Broker, BrokerAsk, BrokerCtx}};
        use crate::a2app::{instances, information_flow, runtime};
        let _review_lock = crate::a2app::effect_review::TEST_LOCK.lock().unwrap();

        // A saved September Search copy, including its historical manual edit,
        // reproduces the installed app path rather than a fresh stock fixture.
        let mut manifest = a2app_core::builtin::stock("search").unwrap();
        manifest.id = "search-saved-upgrade-test".into();
        manifest.source = include_str!("testdata/search_legacy.splash").into();
        a2app_core::persistence::ensure_current_version(&mut manifest,
            a2app_core::versions::VersionOrigin::Manual, "Edited by hand", 1, 0).unwrap();
        a2app_core::persistence::save_user_app(&manifest).unwrap();
        let saved = a2app_core::persistence::load_user_apps().into_iter().find(|saved| saved.id == manifest.id).unwrap();
        let mut current_stock = a2app_core::builtin::stock("search").unwrap();
        current_stock.id = saved.id.clone();
        let upgrade = a2app_core::builtin::reconcile_builtin(&saved, &current_stock, None, None, 2, 0).unwrap();
        assert_eq!(upgrade.manifest.source, saved.source, "upgrading must retain the user's manual edit");
        assert!(upgrade.available_update.is_some());
        let manifest = upgrade.manifest;
        assert!(information_flow::manifest_has_private_source(&manifest));
        let previous_account = information_flow::TEST_ACCOUNT.with(|account| account.replace(Some("@saved-search:test".into())));
        runtime::initialize_background_test(manifest.clone());
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let template = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            makepad_code_editor::script_mod(vm);
            crate::shared::script_mod(vm);
            crate::a2app::host_set::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.MiniAppHost {} });
            vm.bx.heap.new_object_ref(value.as_object().unwrap())
        });
        instances::set_host_template(template);
        let room = matrix_sdk::ruma::OwnedRoomId::try_from("!saved-search:test").unwrap();
        let key = (manifest.id.clone(), Some(room.clone()));
        let host = instances::ensure(&mut cx, &key, &manifest, &[]).unwrap();
        makepad_widgets::widget_tree::set_ui_root(&mut cx, &host);
        for line in manifest.source.lines() {
            if let Some((prefix, _)) = line.split_once(":=")
                && let Some(name) = prefix.split_whitespace().last()
            { host.widget(&cx, &[LiveId::from_str(name)]); }
        }
        let context = instances::context_of_key(&key).unwrap();
        flow::add_sources(&context, [flow::Source::Account { account: context.account().into() }]).unwrap();
        assert!(flow::labels(&context).unwrap().contains(&flow::Source::UnknownPrivate));
        let splash = host.widget(&cx, ids!(splash));
        assert!(splash.borrow_mut::<Splash>().unwrap().call_script_fn_with_strings(&mut cx, id!(run_search), &["nexus"]));
        cx.with_vm_and_async(|_| {});
        let registry = AppRegistry::new(vec![manifest.clone()]);
        let mut store = PermissionStore::default();
        store.set_strict(true);
        let storage = |heap| flow::context_storage_path(&information_flow::context_for_heap(heap)?);
        fn broker_context<'a>(registry: &'a AppRegistry, permissions: &'a PermissionStore, app: &'a str,
            storage: &'a dyn Fn(usize) -> Result<std::path::PathBuf, String>) -> BrokerCtx<'a>
        {
            BrokerCtx { registry, permissions, foreground_app: Some(app),
                is_docked: &|_| true, is_running: &|_| true, pane_state: &|_| None,
                storage_path: storage, room_name: &|_| Some("Saved Search room".into()), desktop_view: true,
                permission_target_room: Some(&runtime::permission_target_room),
                check_flow: &information_flow::check_request, check_response: &information_flow::check_response }
        }
        let mut broker = Broker::new();
        let asks = broker.process(&mut cx, broker_context(&registry, &store, &manifest.id, &storage));
        let mut prompts = asks.into_iter().filter_map(|ask| match ask {
            BrokerAsk::Prompt { request: Some(request), perm, .. } => Some((request, perm)),
            BrokerAsk::FlowReview { .. } => panic!("local search must not review private exports"),
            _ => None,
        }).collect::<Vec<_>>();
        assert_eq!(prompts.len(), 1);
        let (request, permission) = prompts.pop().unwrap();
        store.grant_scoped(&manifest.id, permission, None, RoomScope::room(room.as_str()), GrantDuration::RobrixSession, None).unwrap();
        let asks = broker.dispatch_after_grant(&mut cx, broker_context(&registry, &store, &manifest.id, &storage), request);
        let mut calls = asks.into_iter().filter_map(|ask| match ask {
            BrokerAsk::Matrix { reply, capability, consent, call, args, .. } => Some((reply, capability, consent, call, args)),
            BrokerAsk::Prompt { .. } | BrokerAsk::FlowReview { .. } => panic!("one approval must resume the saved search"),
            _ => None,
        }).collect::<Vec<_>>();
        assert_eq!(calls.len(), 1);
        let (reply, capability, consent, call, args) = calls.pop().unwrap();
        assert_eq!(args["query"], "nexus");
        assert_eq!(args["server"], false);
        let authorization = MatrixAuthorization::new(&manifest.id, capability, Some(room.as_str()), &consent)
            .with_flow(context.clone()).with_payload(args);
        publish_permission_policy(&store);
        with_authorization(authorization.clone(), async {
            review_room_access(room.as_str(), RoomAccess::Read).await.unwrap();
            assert!(room_access_allowed(room.as_str(), RoomAccess::Read));
            assert!(crate::a2app::effect_review::take_pending().is_empty());
        }).await;
        assert!(matches!(call, services::MatrixServiceCall::Search { server: false, .. }));
        let result = super::super::A2AppMatrixResult { reply,
            result: Ok(serde_json::json!({ "results": [{ "room_id": room, "room_name": "Saved Search room",
                "event_id": "$hit:test", "sender": "Alice", "body": "nexus", "ts": 1 }], "searched_rooms": 1, "server_used": false }).to_string()),
            authorization: Some(authorization.clone()), target: Some((room.to_string(), RoomAccess::Read)), reads_rooms: true };
        let response = result.checked_result(&store).unwrap();
        information_flow::check_response(reply, &response).unwrap();
        services::respond(&mut cx, reply, Ok(&response));
        cx.with_vm_and_async(|_| {});
        assert!(host.widget(&cx, ids!(status)).text().starts_with("1 result"));

        // All durations work for the saved app without changing its provenance.
        // Lasting choices cover only this operation and exact server origin.
        for duration in 0..3 {
            use std::{future::Future, task::{Context, Poll, Waker}};
            let parameters = serde_json::json!({ "query": "nexus", "room_ids": [room], "limit": 40 });
            let mut worker = Box::pin(with_authorization(authorization.clone(),
                ensure_server_output_with_payload("https://matrix.example", &parameters)));
            assert!(worker.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
            let mut pending = crate::a2app::effect_review::take_pending();
            assert_eq!(pending.len(), 1);
            assert!(pending[0].allow_once);
            assert!(pending[0].review.allow_session());
            pending[0].approve(|review| match duration {
                0 => flow::approve_effect_once(review),
                1 => flow::approve_effect_session(review, flow::SharingDuration::RobrixSession),
                _ => flow::approve_effect_always(review),
            }).unwrap();
            pending.pop().unwrap().finish(Ok(()));
            assert!(matches!(worker.as_mut().poll(&mut Context::from_waker(Waker::noop())), Poll::Ready(Ok(()))));
            if duration > 0 {
                let changed = serde_json::json!({ "query": "another query", "room_ids": [room], "limit": 40 });
                with_authorization(authorization.clone(), ensure_server_output_with_payload("https://matrix.example", &changed)).await.unwrap();
                assert!(crate::a2app::effect_review::take_pending().is_empty());
                for grant in flow::effect_authorities().unwrap().into_iter().filter(|grant| grant.context == context) {
                    assert_eq!(grant.duration == flow::SharingDuration::Permanent, duration == 2);
                    flow::revoke_effect_authority(grant.id).unwrap();
                }
            }
            assert!(flow::effect_authorities().unwrap().into_iter().all(|grant| grant.context != context));
        }
        assert!(flow::labels(&context).unwrap().contains(&flow::Source::UnknownPrivate));
        instances::quit_app(&mut cx, &manifest.id);
        instances::clear_host_template();
        information_flow::TEST_ACCOUNT.with(|account| { account.replace(previous_account); });
        std::fs::remove_dir_all(a2app_core::data_root().join("apps").join(&manifest.id)).unwrap();
        makepad_widgets::splash_host::take_splash_host_requests();
    }

    #[tokio::test]
    async fn server_query_review_resumes_original_and_obeys_selected_duration() {
        use std::{future::Future, task::{Context, Poll, Waker}};
        let _review_lock = crate::a2app::effect_review::TEST_LOCK.lock().unwrap();
        let context = ContextId::App { account: "@search-once:test".into(), app: "search-once-test".into(), room: Some("!room:test".into()) };
        let previous_account = crate::a2app::information_flow::TEST_ACCOUNT.with(|account| account.replace(Some(context.account().into())));
        flow::register_context(&context).unwrap();
        flow::add_sources(&context, [flow::Source::Account { account: context.account().into() }]).unwrap();
        let mut store = PermissionStore::default();
        store.set("search-once-test", Permission::MatrixRoomRead, GrantState::Granted);
        publish_permission_policy(&store);
        let auth = MatrixAuthorization::new("search-once-test", "matrix.room.messages.search", context.room(), &store).with_flow(context.clone());
        let parameters = serde_json::json!({ "query": "nexus", "room_ids": ["!room:test"], "limit": 40 });
        let mut worker = Box::pin(with_authorization(auth.clone(), ensure_server_output_with_payload("https://matrix.example", &parameters)));
        assert!(worker.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
        let mut pending = crate::a2app::effect_review::take_pending();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].allow_once);
        let capture: serde_json::Value = serde_json::from_str(&pending[0].review.payload).unwrap();
        assert_eq!(capture["operation"], "matrix.room.messages.search");
        assert_eq!(capture["parameters"], parameters);
        pending[0].approve(flow::approve_effect_once).unwrap();
        pending.pop().unwrap().finish(Ok(()));
        assert!(matches!(worker.as_mut().poll(&mut Context::from_waker(Waker::noop())), Poll::Ready(Ok(()))));
        let changed = serde_json::json!({ "query": "different secret", "room_ids": ["!room:test"], "limit": 40 });
        let mut worker = Box::pin(with_authorization(auth.clone(), ensure_server_output_with_payload("https://matrix.example", &changed)));
        assert!(worker.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
        let mut pending = crate::a2app::effect_review::take_pending();
        assert_eq!(pending.len(), 1, "once must not authorize a different query");
        assert!(pending[0].review.payload.contains("different secret"));
        pending.pop().unwrap().finish(Err("Denied".into()));
        assert!(matches!(worker.as_mut().poll(&mut Context::from_waker(Waker::noop())), Poll::Ready(Err(_))));
        for permanent in [false, true] {
            let mut worker = Box::pin(with_authorization(auth.clone(), ensure_server_output_with_payload("https://matrix.example", &parameters)));
            assert!(worker.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
            let mut pending = crate::a2app::effect_review::take_pending();
            assert_eq!(pending.len(), 1);
            pending[0].approve(|review| if permanent { flow::approve_effect_always(review) }
                else { flow::approve_effect_session(review, flow::SharingDuration::RobrixSession) }).unwrap();
            pending.pop().unwrap().finish(Ok(()));
            assert!(matches!(worker.as_mut().poll(&mut Context::from_waker(Waker::noop())), Poll::Ready(Ok(()))));
            with_authorization(auth.clone(), ensure_server_output_with_payload("https://matrix.example", &changed)).await.unwrap();
            assert!(crate::a2app::effect_review::take_pending().is_empty(), "session and forever reuse this operation's review");
            let recipient = Recipient::network_origin("https://matrix.example").unwrap();
            assert!(flow::ensure_allowed(&context, &recipient).is_err(), "approval must not grant arbitrary exports");
            for grant in flow::effect_authorities().unwrap().into_iter().filter(|grant| grant.context == context) {
                assert_eq!(grant.duration == flow::SharingDuration::Permanent, permanent);
                flow::revoke_effect_authority(grant.id).unwrap();
            }
        }
        flow::remove_context(&context).unwrap();
        crate::a2app::information_flow::TEST_ACCOUNT.with(|account| { account.replace(previous_account); });
    }

    // Each Tokio test owns a separate runtime/test thread. Keep the global
    // policy guard through authorization awaits so another test cannot replace it.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn directory_hierarchy_query_uses_captured_scope_and_current_consent() {
        let _review_lock = crate::a2app::effect_review::TEST_LOCK.lock().unwrap();
        let account = format!("@directory-query-{}:test", std::process::id());
        let previous_account = crate::a2app::information_flow::TEST_ACCOUNT.with(|a| a.replace(Some(account)));
        let context = crate::a2app::information_flow::begin_agent_session("!directory-query:test").unwrap();
        crate::a2app::information_flow::record_directory_response(&context).unwrap();
        let recipient = Recipient::network_origin("https://directory-query.test").unwrap();
        let mut grants = Vec::new();
        for source in flow::labels(&context).unwrap() {
            grants.push(flow::grant_sharing(source, recipient.clone(), flow::ReaderScope::Context(context.clone()),
                flow::SharingDuration::RobrixSession).unwrap());
        }
        let subject = a2app_core::permissions::agent_subject(context.room().unwrap());
        let mut store = PermissionStore::default();
        store.grant_scoped(&subject, Permission::MatrixSpaces, Some("matrix.space.rooms.list"),
            RoomScope::room("!child:test"), GrantDuration::Always, None).unwrap();
        let mut auth = MatrixAuthorization::new(&subject, "matrix.space.rooms.list", context.room(), &store)
            .with_flow(context.clone()).with_payload(serde_json::json!({ "space_id": "!space:test" }));
        auth.target_room = Some("!space:test".into());
        publish_permission_policy(&store);
        with_authorization(auth.clone(), async {
            assert!(auth.permits_request(&store));
            assert!(!auth.permits(&store, Some("!space:test")), "the root itself is outside the selected child scope");
            let query = begin_server_query("https://directory-query.test").await.unwrap();
            query.check().unwrap();
            store.set(&subject, Permission::MatrixSpaces, GrantState::Denied);
            publish_permission_policy(&store);
            assert!(query.check().is_err(), "a denied directory permission cancels later hierarchy pages");
            assert!(begin_server_query("https://directory-query.test").await.is_err());
        }).await;
        publish_permission_policy(&PermissionStore::default());
        for grant in grants { flow::revoke_sharing(grant).unwrap(); }
        flow::remove_context(&context).unwrap();
        crate::a2app::information_flow::TEST_ACCOUNT.with(|a| { a.replace(previous_account); });
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
