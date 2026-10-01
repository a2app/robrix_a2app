//! Combined, host-captured sharing and action approval for one final effect.

use super::*;
use crate::permissions::RoomScope;
use sha2::{Digest, Sha256};

const MAX_PENDING_EFFECTS: usize = 64;
pub const EFFECT_REVIEW_REQUIRED: &str = "Your approval is needed before this request can continue.";

/// Trusted live room membership; never resolve space scopes from app input.
pub type EffectRoomScopeMatcher = fn(&str, &RoomScope, &str) -> bool;

/// A reviewed operation for one app and its selected destinations.
///
/// This does not create a general sharing rule or action authority. Room
/// inputs and targets can vary within the chosen scope; other inputs,
/// capabilities and destinations retain their exact reviewed scope.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectAuthority {
    pub id: u64,
    pub context: ContextId,
    pub recipient: Option<Recipient>,
    pub action: Option<SensitiveAction>,
    pub operation: String,
    pub sources: Label,
    pub influences: Influences,
    pub duration: SharingDuration,
    /// Absent in older metadata: preserve the original exact context scope.
    #[serde(default)]
    pub room_scope: Option<RoomScope>,
}

fn normalize_scope(mut scope: RoomScope) -> Result<RoomScope, String> {
    if let RoomScope::Selection { rooms, spaces } = &mut scope {
        for ids in [rooms, spaces] {
            for id in ids.iter_mut() { *id = id.trim().to_string(); }
            ids.retain(|id| !id.is_empty());
            ids.sort(); ids.dedup();
        }
        if let RoomScope::Selection { rooms, spaces } = &scope
            && rooms.is_empty() && spaces.is_empty()
        { return Err("Choose at least one room or space.".into()); }
    }
    Ok(scope)
}

fn room_action(action: &SensitiveAction, context: &ContextId, _sources: &Label, recipient: Option<&Recipient>) -> bool {
    use crate::capabilities::FlowOutput;
    let Some(capability) = crate::capabilities::by_id(&action.kind) else { return false };
    let Some(contract) = capability.flow_contract() else { return false };
    if contract.output == FlowOutput::TargetRoom {
        return matches!(recipient, Some(Recipient::MatrixRoom { account, room })
            if account == context.account() && room == &action.target);
    }
    // These targets are resolved and validated by the host navigation route;
    // opening a room does not require having read its contents beforehand.
    matches!(capability.id, "host.nav.event" | "host.nav.thread" | "host.nav.link")
        && action.target.starts_with('!')
}

/// Actual operation targets are independent of the context's historical data.
fn scope_targets(context: &ContextId, sources: &Label, recipient: Option<&Recipient>, action: Option<&SensitiveAction>, payload: &serde_json::Value) -> Result<BTreeSet<String>, String> {
    use crate::capabilities::Scope;
    let mut targets = BTreeSet::new();
    if let Some(Recipient::MatrixRoom { room, .. }) = recipient { targets.insert(room.clone()); }
    if let Some(action) = action.filter(|action| room_action(action, context, sources, recipient)) {
        targets.insert(action.target.clone());
    }
    let mut add = |value: &serde_json::Value| -> Result<(), String> {
        let room = value.as_str().filter(|room| !room.is_empty()).ok_or("Missing scoped room target.")?;
        targets.insert(room.into());
        Ok(())
    };
    // Sensitive actions and recipients come from the final host operation.
    // A guest's extra scope_targets field must never suppress those targets.
    let capability = action.and_then(|action| crate::capabilities::by_id(&action.kind))
        .or_else(|| payload["operation"].as_str().and_then(crate::capabilities::by_id));
    let Some(capability) = capability.filter(|capability| matches!(capability.scope, Scope::Room | Scope::MultiRoom | Scope::Space)) else { return Ok(targets) };
    let parameters = payload.get("parameters").unwrap_or(payload);
    for key in ["room_ids", "rooms"] {
        if let Some(values) = parameters.get(key) {
            for value in values.as_array().ok_or("Invalid scoped room targets.")? { add(value)?; }
        }
    }
    for key in ["room_id", "space_id", "room"] {
        if let Some(room) = parameters.get(key) {
            add(room)?;
        }
    }
    if capability.scope == Scope::Room && targets.is_empty() && let Some(room) = context.room() {
        targets.insert(room.into());
    }
    Ok(targets)
}

fn operation_key(action: Option<&SensitiveAction>, payload: &serde_json::Value) -> Result<String, String> {
    if let Some(operation) = payload["operation"].as_str() {
        if operation.is_empty() || operation.len() > 256 { return Err("The operation to approve is invalid.".into()); }
        return Ok(format!("operation:{operation}"));
    }
    if let Some(action) = action { return Ok(format!("action:{}", action.kind)); }
    // Earlier host callers did not identify a read-only operation. Keep their
    // approval limited to identical arguments; no private text is persisted.
    let digest = Sha256::digest(integrity::canonical_payload(payload)?.as_bytes());
    Ok(format!("request:{digest:x}"))
}

pub(super) fn validate_authority(grant: &EffectAuthority) -> Result<(), String> {
    validate_context(&grant.context)?;
    if let Some(recipient) = &grant.recipient { recipient.validate()?; }
    if let Some(action) = &grant.action { integrity::validate_action(action)?; }
    if grant.operation.is_empty() || grant.operation.len() > 512 { return Err("Invalid approved operation.".into()); }
    for source in &grant.sources {
        validate_source(source)?;
        if matches!(source, Source::Account { account } | Source::Room { account, .. } if account != grant.context.account()) {
            return Err("Operation approval must belong to the account whose data it uses.".into());
        }
    }
    if grant.recipient == Some(Recipient::External) && !grant.sources.is_empty() {
        return Err("Private data approval needs a specific destination.".into());
    }
    if let Some(scope) = &grant.room_scope {
        if normalize_scope(scope.clone())? != *scope { return Err("Invalid approved room selection.".into()); }
        if matches!(grant.duration, SharingDuration::RoomSession { .. }) {
            return Err("A scoped operation approval must use the Robrix session or Forever.".into());
        }
    }
    for influence in &grant.influences { integrity::validate_influence(influence)?; }
    if let SharingDuration::RoomSession { account, room } = &grant.duration
        && (account != grant.context.account() || grant.context.room() != Some(room.as_str()))
    { return Err("Operation approval must expire with its own room session.".into()); }
    Ok(())
}

/// Immutable review information held only in bounded host memory.
#[derive(Clone, Eq, PartialEq)]
pub struct EffectReview {
    pub id: u64,
    pub context: ContextId,
    pub epoch: u64,
    pub recipient: Option<Recipient>,
    pub action: Option<SensitiveAction>,
    pub sources: Label,
    pub denied_sources: Label,
    pub influences: Influences,
    pub payload: std::sync::Arc<str>,
    pub allowed: bool,
}

impl std::fmt::Debug for EffectReview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EffectReview").field("id", &self.id).field("context", &self.context)
            .field("epoch", &self.epoch).field("recipient", &self.recipient).field("action", &self.action)
            .field("sources", &self.sources).field("influences", &self.influences).field("allowed", &self.allowed)
            .finish_non_exhaustive()
    }
}

pub(super) struct PendingEffect {
    pub review: EffectReview,
    approved_once: bool,
}

impl EffectReview {
    /// Lasting approvals cover this app's operation and reviewed inputs only.
    ///
    /// Legacy labels stay private; approving this operation does not create
    /// a general sharing rule for unidentified data.
    pub fn allow_session(&self) -> bool {
        self.sources.iter().all(|source| match source {
            Source::Account { account } | Source::Room { account, .. } | Source::RoomDirectory { account } => account == self.context.account(),
            Source::UnknownPrivate => true,
        })
    }

    fn same_capture(&self, other: &Self) -> bool {
        self.id == other.id && self.context == other.context && self.epoch == other.epoch
            && self.recipient == other.recipient && self.action == other.action
            && self.sources == other.sources && self.denied_sources == other.denied_sources
            && self.influences == other.influences && self.payload == other.payload
    }
}

impl Registry {
    /// Install a host callback backed by current, revision-checked ancestry.
    pub fn set_effect_room_scope_matcher(&mut self, matcher: EffectRoomScopeMatcher) -> Result<(), String> {
        self.check_healthy()?;
        self.effect_room_scope_matcher = Some(matcher);
        Ok(())
    }

    fn effect_scope_matches(&self, account: &str, scope: &RoomScope, room: &str) -> bool {
        match scope {
            RoomScope::AllRooms => true,
            RoomScope::Selection { rooms, spaces } => rooms.iter().any(|id| id == room)
                || spaces.iter().any(|space| space == room)
                || (!spaces.is_empty() && self.effect_room_scope_matcher.is_some_and(|matches| matches(account, scope, room))),
        }
    }

    fn effect_authority_matches(&self, grant: &EffectAuthority, context: &ContextId, recipient: Option<&Recipient>, action: Option<&SensitiveAction>, operation: &str, sources: &Label, influences: &Influences, payload: &serde_json::Value) -> bool {
        if grant.operation != operation { return false; }
        let Some(scope) = &grant.room_scope else {
            return grant.context == *context && grant.recipient.as_ref() == recipient && grant.action.as_ref() == action
                && sources.is_subset(&grant.sources) && influences.is_subset(&grant.influences);
        };
        let account = grant.context.account();
        let matches_room = |room: &str| self.effect_scope_matches(account, scope, room);
        if grant.context != *context && !matches!((&grant.context, context),
            (ContextId::App { account: a, app: app_a, room: Some(_) }, ContextId::App { account: b, app: app_b, room: Some(room) })
            if a == b && app_a == app_b && matches_room(room))
        { return false; }
        if grant.recipient.as_ref() != recipient && !matches!((grant.recipient.as_ref(), recipient),
            (Some(Recipient::MatrixRoom { account: a, .. }), Some(Recipient::MatrixRoom { account: b, room }))
            if a == b && a == account && matches_room(room))
        { return false; }
        if grant.action.as_ref() != action && !matches!((grant.action.as_ref(), action), (Some(original), Some(current))
            if original.kind == current.kind && room_action(original, &grant.context, &grant.sources, grant.recipient.as_ref())
                && room_action(current, context, sources, recipient) && matches_room(&current.target))
        { return false; }
        if !sources.iter().all(|source| grant.sources.contains(source) || matches!(source,
            Source::Room { account: source_account, room } if source_account == account && matches_room(room)))
            || !influences.iter().all(|influence| grant.influences.contains(influence) || matches!(influence,
                Influence::RoomContent { account: source_account, room } if source_account == account && matches_room(room)))
        { return false; }
        scope_targets(context, sources, recipient, action, payload)
            .is_ok_and(|targets| targets.iter().all(|room| matches_room(room)))
    }

    /// Capture the complete final operation after ordinary capability checks.
    ///
    /// Apps cannot supply their own recipient, operation, activation, sources,
    /// or influences. The host resolves them before showing this review.
    pub fn prepare_effect_for_activation(&mut self, context: &ContextId, epoch: u64, recipient: Option<&Recipient>, action: Option<&SensitiveAction>, payload: &serde_json::Value) -> Result<EffectReview, String> {
        self.ensure_context_epoch(context, epoch)?;
        let sources = self.labels(context)?;
        let influences = self.influences(context)?;
        if let Some(recipient) = recipient { recipient.validate()?; }
        if let Some(action) = action { integrity::validate_action(action)?; }
        let denied_sources = if let Some(recipient) = recipient { self.decision(context, recipient)?.denied_sources } else { Label::new() };
        let action_allowed = action.map(|action| self.action_decision(context, action).map(|decision| decision.allowed)).transpose()?.unwrap_or(true);
        let reviewed_allowed = if denied_sources.is_empty() && action_allowed { true } else {
            let operation = operation_key(action, payload)?;
            self.metadata.effect_authorities.iter().chain(self.session_effect_authorities.iter()).any(|grant|
                self.effect_authority_matches(grant, context, recipient, action, &operation, &sources, &influences, payload))
        };
        if reviewed_allowed {
            self.pending_effects.retain(|pending| pending.review.context != *context || pending.review.epoch != epoch
                || pending.review.recipient.as_ref() != recipient || pending.review.action.as_ref() != action);
            return Ok(EffectReview { id: 0, context: context.clone(), epoch, recipient: recipient.cloned(), action: action.cloned(),
                sources, denied_sources, influences, payload: "".into(), allowed: true });
        }
        if recipient == Some(&Recipient::External) && !denied_sources.is_empty() { return Err("This request has no specific destination and cannot share private data through a permission prompt.".into()); }
        let payload: std::sync::Arc<str> = integrity::canonical_payload(payload)?.into();
        let mut review = EffectReview { id: 0, context: context.clone(), epoch, recipient: recipient.cloned(), action: action.cloned(),
            sources, denied_sources, influences, payload, allowed: false };
        let existing = self.pending_effects.iter().find(|pending| {
            review.id = pending.review.id;
            review.same_capture(&pending.review)
        });
        if let Some(pending) = existing {
            review.id = pending.review.id;
            review.allowed = (review.denied_sources.is_empty() && action_allowed) || pending.approved_once;
            return Ok(review);
        }
        review.id = self.ephemeral_id()?;
        review.allowed = review.denied_sources.is_empty() && action_allowed;
        if self.pending_effects.len() == MAX_PENDING_EFFECTS { self.pending_effects.pop_front(); }
        self.pending_effects.push_back(PendingEffect { review: review.clone(), approved_once: false });
        Ok(review)
    }

    pub fn check_effect_for_activation(&mut self, context: &ContextId, epoch: u64, recipient: Option<&Recipient>, action: Option<&SensitiveAction>, payload: &serde_json::Value) -> Result<(), String> {
        let review = self.prepare_effect_for_activation(context, epoch, recipient, action, payload)?;
        if review.allowed { Ok(()) } else { Err(EFFECT_REVIEW_REQUIRED.into()) }
    }

    fn reviewed_effect(&self, expected: &EffectReview) -> Result<(), String> {
        self.ensure_context_epoch(&expected.context, expected.epoch)?;
        if self.labels(&expected.context)? != expected.sources || self.influences(&expected.context)? != expected.influences {
            return Err("This app received new data. Review the request again before allowing it.".into());
        }
        if let Some(recipient) = &expected.recipient
            && self.decision(&expected.context, recipient)?.denied_sources != expected.denied_sources
        { return Err("Sharing permissions changed. Review this request again before allowing it.".into()); }
        if !self.pending_effects.iter().any(|pending| pending.review.same_capture(expected)) {
            return Err("This request expired, was cancelled, or already completed.".into());
        }
        if expected.denied_sources.iter().any(|source| match source {
            Source::Account { account } | Source::Room { account, .. } | Source::RoomDirectory { account } => account != expected.context.account(),
            Source::UnknownPrivate => false,
        }) { return Err("This request contains private data that cannot be approved from the current account.".into()); }
        Ok(())
    }

    /// One immutable operation only; creates no source or session sharing rule.
    pub fn approve_effect_once(&mut self, expected: &EffectReview) -> Result<(), String> {
        self.reviewed_effect(expected)?;
        self.pending_effects.iter_mut().find(|pending| pending.review.id == expected.id).unwrap().approved_once = true;
        Ok(())
    }

    /// Approve this operation, target and reviewed inputs in one transaction.
    ///
    /// The UI must explain that session permission also covers other contents.
    /// Future additional influences still require a new integrity review.
    pub fn approve_effect_session(&mut self, expected: &EffectReview, duration: SharingDuration) -> Result<(), String> {
        match &duration {
            SharingDuration::RobrixSession => {}
            SharingDuration::RoomSession { account, room } if account == expected.context.account() && expected.context.room() == Some(room.as_str()) => {}
            _ => return Err("This request can only be approved for this Robrix session or its own room session.".into()),
        }
        self.approve_effect_duration(expected, duration)
    }

    /// Persist this app's reviewed operation without broadening sharing rules.
    pub fn approve_effect_always(&mut self, expected: &EffectReview) -> Result<(), String> {
        self.approve_effect_duration(expected, SharingDuration::Permanent)
    }

    /// Approve this app operation for the room/space selection shown by the host.
    ///
    /// Non-room destinations and influences retain their exact reviewed floor.
    pub fn approve_effect_scoped(&mut self, expected: &EffectReview, scope: RoomScope, duration: SharingDuration) -> Result<(), String> {
        if !matches!(duration, SharingDuration::RobrixSession | SharingDuration::Permanent) {
            return Err("Choose Until you quit Robrix or Forever for a room selection.".into());
        }
        self.approve_effect_duration_scoped(expected, duration, Some(normalize_scope(scope)?))
    }

    fn approve_effect_duration(&mut self, expected: &EffectReview, duration: SharingDuration) -> Result<(), String> {
        self.approve_effect_duration_scoped(expected, duration, None)
    }

    fn approve_effect_duration_scoped(&mut self, expected: &EffectReview, duration: SharingDuration, room_scope: Option<RoomScope>) -> Result<(), String> {
        self.reviewed_effect(expected)?;
        if !expected.allow_session() { return Err("A lasting approval cannot cover another account's data.".into()); }
        let payload = serde_json::from_str(&expected.payload).map_err(|_| "The reviewed request is invalid.")?;
        if let Some(scope) = &room_scope {
            let targets = scope_targets(&expected.context, &expected.sources, expected.recipient.as_ref(), expected.action.as_ref(), &payload)?;
            if !targets.iter().all(|room| self.effect_scope_matches(expected.context.account(), scope, room)) {
                return Err("The room selection does not include this request's destination or accessed rooms.".into());
            }
        }
        let mut grant = EffectAuthority { id: 0, context: expected.context.clone(), recipient: expected.recipient.clone(),
            action: expected.action.clone(), operation: operation_key(expected.action.as_ref(), &payload)?,
            sources: expected.sources.clone(), influences: expected.influences.clone(), duration, room_scope };
        validate_authority(&grant)?;
        if grant.duration == SharingDuration::Permanent {
            let mut next = self.metadata.clone();
            grant.id = storage::next_grant_id(&mut next)?;
            next.effect_authorities.retain(|old| old.context != grant.context || old.recipient != grant.recipient
                || old.action != grant.action || old.operation != grant.operation || old.room_scope != grant.room_scope);
            next.effect_authorities.push(grant);
            self.persist(next)?;
        } else {
            grant.id = self.ephemeral_id()?;
            self.session_effect_authorities.retain(|old| old.context != grant.context || old.recipient != grant.recipient
                || old.action != grant.action || old.operation != grant.operation || old.duration != grant.duration || old.room_scope != grant.room_scope);
            self.session_effect_authorities.push(grant);
        }
        self.pending_effects.retain(|pending| pending.review.id != expected.id);
        Ok(())
    }

    pub fn effect_authorities(&self) -> Result<Vec<EffectAuthority>, String> {
        self.check_healthy()?;
        Ok(self.metadata.effect_authorities.iter().chain(self.session_effect_authorities.iter()).cloned().collect())
    }

    pub fn revoke_effect_authority(&mut self, id: u64) -> Result<bool, String> {
        self.check_healthy()?;
        if self.session_effect_authorities.iter().any(|grant| grant.id == id) {
            self.session_effect_authorities.retain(|grant| grant.id != id);
            return Ok(true);
        }
        if !self.metadata.effect_authorities.iter().any(|grant| grant.id == id) { return Ok(false); }
        let mut next = self.metadata.clone();
        next.effect_authorities.retain(|grant| grant.id != id);
        self.persist(next)?;
        Ok(true)
    }

    /// Consume once approval under the registry lock immediately before effect.
    ///
    /// Every capability, deny rule and protected-room check remains the host's
    /// responsibility at this boundary. A failed effect never refunds approval.
    pub fn commit_effect_for_activation(&mut self, context: &ContextId, epoch: u64, recipient: Option<&Recipient>, action: Option<&SensitiveAction>, payload: &serde_json::Value) -> Result<(), String> {
        let review = self.prepare_effect_for_activation(context, epoch, recipient, action, payload)?;
        if !review.allowed { return Err(EFFECT_REVIEW_REQUIRED.into()); }
        self.pending_effects.retain(|pending| pending.review.id != review.id);
        Ok(())
    }

    pub fn cancel_effect(&mut self, id: u64) -> Result<bool, String> {
        self.check_healthy()?;
        let before = self.pending_effects.len();
        self.pending_effects.retain(|pending| pending.review.id != id);
        Ok(before != self.pending_effects.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::tests::TestRoot;

    fn context() -> ContextId { ContextId::App { account: "alice".into(), app: "test".into(), room: Some("room-a".into()) } }
    fn recipient() -> Recipient { Recipient::NetworkOrigin("https://example.com".into()) }
    fn action() -> SensitiveAction { SensitiveAction { kind: "network.POST".into(), target: "https://example.com".into() } }
    fn private(registry: &mut Registry, context: &ContextId) {
        registry.register_context(context).unwrap();
        registry.add_sources(context, [Source::Account { account: "alice".into() }, Source::Room { account: "alice".into(), room: "room-a".into() }]).unwrap();
    }

    #[test]
    fn one_approval_combines_all_sources_and_integrity_without_session_rules() {
        let root = TestRoot::new(); let mut registry = root.registry(); let context = context();
        private(&mut registry, &context); let epoch = registry.context_epoch(&context).unwrap();
        let payload = serde_json::json!({"body":"private text","url":"https://example.com/post"});
        let review = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), Some(&action()), &payload).unwrap();
        assert!(!review.allowed); assert_eq!(review.denied_sources.len(), 2);
        registry.approve_effect_once(&review).unwrap();
        registry.check_effect_for_activation(&context, epoch, Some(&recipient()), Some(&action()), &payload).unwrap();
        assert!(registry.sharing_grants().unwrap().is_empty()); assert!(registry.authorities().unwrap().is_empty());
        assert!(registry.ensure_allowed(&context, &recipient()).is_err());
        registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), Some(&action()), &payload).unwrap();
        assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), Some(&action()), &payload).is_err());
        assert!(!format!("{review:?}").contains("private text"));
        assert!(!fs::read_to_string(root.0.join(METADATA_FILE)).unwrap().contains("private text"));
    }

    #[test]
    fn sharing_only_once_is_consumed_and_changed_contents_or_sources_need_review() {
        let root = TestRoot::new(); let mut registry = root.registry(); let context = context();
        private(&mut registry, &context); let epoch = registry.context_epoch(&context).unwrap();
        let payload = serde_json::json!({"url":"https://example.com/private"});
        let review = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), None, &payload).unwrap();
        registry.approve_effect_once(&review).unwrap();
        assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), None, &serde_json::json!({"url":"https://example.com/changed"})).is_err());
        registry.add_influences(&context, [Influence::InternetOrigin("https://other.example".into())]).unwrap();
        assert!(registry.approve_effect_once(&review).is_err());
        assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), None, &payload).is_err());
        let fresh = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), None, &payload).unwrap();
        registry.approve_effect_once(&fresh).unwrap();
        registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), None, &payload).unwrap();
        assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), None, &payload).is_err());
    }

    #[test]
    fn explicit_session_review_is_atomic_and_new_influences_require_review() {
        let root = TestRoot::new(); let mut registry = root.registry(); let context = context();
        private(&mut registry, &context); let epoch = registry.context_epoch(&context).unwrap(); let payload = serde_json::json!({"body":"one"});
        let review = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), Some(&action()), &payload).unwrap();
        assert!(registry.approve_effect_session(&review, SharingDuration::Permanent).is_err());
        assert!(registry.sharing_grants().unwrap().is_empty()); assert!(registry.authorities().unwrap().is_empty());
        registry.approve_effect_session(&review, SharingDuration::RobrixSession).unwrap();
        assert!(registry.sharing_grants().unwrap().is_empty()); assert!(registry.authorities().unwrap().is_empty());
        assert_eq!(registry.effect_authorities().unwrap().len(), 1);
        registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), Some(&action()), &serde_json::json!({"body":"two"})).unwrap();
        registry.add_influences(&context, [Influence::Unknown]).unwrap();
        assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), Some(&action()), &payload).is_err());
        registry.end_session().unwrap(); registry.register_context(&context).unwrap();
        assert!(registry.ensure_allowed(&context, &recipient()).is_err());
    }

    #[test]
    fn saved_read_approval_is_bound_to_operation_destination_and_reviewed_inputs() {
        for permanent in [false, true] {
            let root = TestRoot::new(); let mut registry = root.registry(); let context = context();
            private(&mut registry, &context); let epoch = registry.context_epoch(&context).unwrap();
            let payload = serde_json::json!({"operation":"matrix.room.messages.search","term":"private query"});
            let review = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), None, &payload).unwrap();
            if permanent { registry.approve_effect_always(&review).unwrap(); }
            else { registry.approve_effect_session(&review, SharingDuration::RobrixSession).unwrap(); }
            let changed = serde_json::json!({"operation":"matrix.room.messages.search","term":"another query"});
            registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), None, &changed).unwrap();
            assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), None,
                &serde_json::json!({"operation":"matrix.user.profile.read","term":"another query"})).is_err());
            assert!(registry.commit_effect_for_activation(&context, epoch, Some(&Recipient::NetworkOrigin("https://other.example".into())), None, &changed).is_err());
            assert!(registry.ensure_allowed(&context, &recipient()).is_err(), "operation approval must not become a source-wide sharing rule");
            assert!(registry.sharing_grants().unwrap().is_empty());
            registry.add_sources(&context, [Source::Room { account: "alice".into(), room: "room-b".into() }]).unwrap();
            assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), None, &changed).is_err());
            let review = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), None, &changed).unwrap();
            if permanent { registry.approve_effect_always(&review).unwrap(); }
            else { registry.approve_effect_session(&review, SharingDuration::RobrixSession).unwrap(); }
            registry.add_influences(&context, [Influence::InternetOrigin("https://new.example".into())]).unwrap();
            assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), None, &changed).is_err());
            assert!(!fs::read_to_string(root.0.join(METADATA_FILE)).unwrap().contains("private query"));
        }
    }

    #[test]
    fn forever_survives_restart_but_revoke_and_other_contexts_still_require_approval() {
        let root = TestRoot::new(); let context = context();
        let payload = serde_json::json!({"body":"private original contents"});
        let id = {
            let mut registry = root.registry(); private(&mut registry, &context);
            let epoch = registry.context_epoch(&context).unwrap();
            let review = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), Some(&action()), &payload).unwrap();
            registry.approve_effect_always(&review).unwrap();
            registry.end_session().unwrap();
            assert_eq!(registry.effect_authorities().unwrap().len(), 1);
            registry.effect_authorities().unwrap()[0].id
        };
        let mut registry = root.registry(); registry.register_context(&context).unwrap();
        let epoch = registry.context_epoch(&context).unwrap();
        registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), Some(&action()),
            &serde_json::json!({"body":"different reviewed-operation contents"})).unwrap();
        let other = ContextId::App { account: "alice".into(), app: "other-app".into(), room: Some("room-a".into()) };
        private(&mut registry, &other);
        let other_epoch = registry.context_epoch(&other).unwrap();
        assert!(registry.commit_effect_for_activation(&other, other_epoch, Some(&recipient()), Some(&action()), &payload).is_err());
        let changed_action = SensitiveAction { kind: "network.DELETE".into(), target: "https://example.com".into() };
        assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), Some(&changed_action), &payload).is_err());
        assert!(registry.revoke_effect_authority(id).unwrap());
        assert!(!registry.revoke_effect_authority(id).unwrap());
        assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), Some(&action()), &payload).is_err());
        assert!(root.registry().effect_authorities().unwrap().is_empty());
        assert!(!fs::read_to_string(root.0.join(METADATA_FILE)).unwrap().contains("private original contents"));
    }

    #[test]
    fn legacy_read_approvals_store_only_a_digest_and_never_cover_changed_arguments() {
        let root = TestRoot::new(); let mut registry = root.registry(); let context = context();
        private(&mut registry, &context); let epoch = registry.context_epoch(&context).unwrap();
        let payload = serde_json::json!({"term":"private query"});
        let review = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), None, &payload).unwrap();
        registry.approve_effect_always(&review).unwrap();
        registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), None, &payload).unwrap();
        assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), None, &serde_json::json!({"term":"different query"})).is_err());
        assert!(!fs::read_to_string(root.0.join(METADATA_FILE)).unwrap().contains("private query"));
    }

    #[test]
    fn old_metadata_defaults_to_no_approval_and_saved_operation_scopes_are_validated() {
        let root = TestRoot::new(); let mut metadata = Metadata::default();
        let mut value = serde_json::to_value(&metadata).unwrap(); value.as_object_mut().unwrap().remove("effect_authorities");
        let decoded = storage::decode(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(decoded.effect_authorities.is_empty());
        let id = storage::next_grant_id(&mut metadata).unwrap();
        metadata.effect_authorities.push(EffectAuthority { id, context: context(),
            recipient: Some(recipient()), action: Some(action()), operation: "action:network.POST".into(),
            sources: [Source::UnknownPrivate].into(), influences: Influences::new(), duration: SharingDuration::Permanent, room_scope: None });
        assert!(storage::validate_metadata(&metadata).is_ok());
        fs::write(root.0.join(METADATA_FILE), serde_json::to_vec(&metadata).unwrap()).unwrap();
        assert!(Registry::open(&root.0).is_ok());
        metadata.effect_authorities[0].recipient = Some(Recipient::External);
        assert!(storage::validate_metadata(&metadata).is_err(), "private approvals need a concrete destination");
        metadata.effect_authorities[0].recipient = Some(recipient());
        metadata.effect_authorities[0].sources = [Source::Account { account: "bob".into() }].into();
        assert!(storage::validate_metadata(&metadata).is_err(), "a saved approval cannot belong to another account's data");
    }

    #[test]
    fn repeated_workers_keep_session_choices_but_never_reuse_exact_approval() {
        for duration in [SharingDuration::RobrixSession,
            SharingDuration::RoomSession { account: "alice".into(), room: "room-a".into() }]
        {
            let root = TestRoot::new(); let mut registry = root.registry(); let context = context();
            private(&mut registry, &context);
            registry.add_influences(&context, [Influence::InternetOrigin("https://example.com".into())]).unwrap();
            let epoch = registry.context_epoch(&context).unwrap();
            let payload = serde_json::json!({"body":"first scheduled report"});
            let review = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), Some(&action()), &payload).unwrap();
            registry.approve_effect_session(&review, duration.clone()).unwrap();
            let copy = SensitiveAction { kind: "device.clipboard.write".into(), target: "clipboard".into() };
            let once = registry.prepare_effect_for_activation(&context, epoch, Some(&Recipient::Clipboard), Some(&copy), &payload).unwrap();
            registry.approve_effect_once(&once).unwrap();

            registry.remove_context(&context);
            assert!(registry.ensure_context_epoch(&context, epoch).is_err());
            registry.register_context(&context).unwrap();
            let next = registry.context_epoch(&context).unwrap();
            assert_ne!(next, epoch);
            registry.commit_effect_for_activation(&context, next, Some(&recipient()), Some(&action()),
                &serde_json::json!({"body":"next scheduled report"})).unwrap();
            assert!(registry.commit_effect_for_activation(&context, next, Some(&Recipient::Clipboard), Some(&copy), &payload).is_err());
            assert!(registry.approve_effect_once(&once).is_err());
            registry.add_influences(&context, [Influence::InternetOrigin("https://new.example".into())]).unwrap();
            assert!(registry.commit_effect_for_activation(&context, next, Some(&recipient()), Some(&action()), &payload).is_err(),
                "the prior session choice covers only the influences the user reviewed");
            registry.close_room_session("alice", "room-a").unwrap();
            assert_eq!(registry.effect_authorities().unwrap().is_empty(), duration != SharingDuration::RobrixSession);
            registry.end_session().unwrap();
            assert!(registry.authorities().unwrap().is_empty());
            assert!(registry.effect_authorities().unwrap().is_empty());
            assert!(registry.sharing_grants().unwrap().is_empty());
        }
    }

    #[test]
    fn restart_cancellation_eviction_and_unreviewable_destinations_fail_closed() {
        let root = TestRoot::new(); let mut registry = root.registry(); let context = context();
        private(&mut registry, &context); let epoch = registry.context_epoch(&context).unwrap(); let payload = serde_json::json!({"body":"one"});
        let review = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), Some(&action()), &payload).unwrap();
        registry.approve_effect_once(&review).unwrap(); registry.cancel_effect(review.id).unwrap();
        assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), Some(&action()), &payload).is_err());
        let evicted = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), None, &payload).unwrap();
        registry.approve_effect_once(&evicted).unwrap();
        for index in 0..MAX_PENDING_EFFECTS { registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), None, &serde_json::json!({"index":index})).unwrap(); }
        assert!(registry.approve_effect_once(&evicted).is_err());
        registry.remove_context(&context); registry.register_context(&context).unwrap();
        assert!(registry.approve_effect_once(&review).is_err());
        assert!(registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), None, &payload).is_err());
        let epoch = registry.context_epoch(&context).unwrap();
        assert!(registry.prepare_effect_for_activation(&context, epoch, Some(&Recipient::External), None, &payload).is_err());
        registry.add_sources(&context, [Source::UnknownPrivate]).unwrap();
        let review = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), None, &payload).unwrap();
        assert!(!review.allowed); assert!(review.allow_session());
    }

    #[test]
    fn ordinary_local_work_and_public_exports_do_not_create_tickets() {
        let root = TestRoot::new(); let mut registry = root.registry(); let context = context();
        registry.register_context(&context).unwrap(); let epoch = registry.context_epoch(&context).unwrap();
        let payload = serde_json::json!({"body":"x".repeat(128 * 1024)});
        let public_export = registry.prepare_effect_for_activation(&context, epoch, Some(&Recipient::External), Some(&action()), &payload).unwrap();
        assert!(public_export.allowed); assert_eq!(public_export.id, 0);
        registry.add_sources(&context, [Source::UnknownPrivate]).unwrap();
        for _ in 0..100 {
            let local = registry.prepare_effect_for_activation(&context, epoch, None, None, &payload).unwrap();
            assert!(local.allowed); assert_eq!(local.id, 0);
        }
        assert!(registry.pending_effects.is_empty());
        let local = registry.prepare_effect_for_activation(&context, epoch, None, Some(&action()), &serde_json::json!({"screen":"rooms"})).unwrap();
        assert!(!local.allowed);
        registry.approve_effect_once(&local).unwrap();
        registry.commit_effect_for_activation(&context, epoch, None, Some(&action()), &serde_json::json!({"screen":"rooms"})).unwrap();
        for recipient in [recipient(), Recipient::Clipboard, Recipient::External] {
            let review = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient), Some(&action()), &serde_json::json!({"body":"private"}));
            if recipient == Recipient::External { assert!(review.is_err()); }
            else { let review = review.unwrap(); assert!(!review.allowed); assert!(review.allow_session()); }
        }
    }

    #[test]
    fn one_time_legacy_approval_covers_only_the_exact_visible_request() {
        let root = TestRoot::new(); let mut registry = root.registry(); let context = context();
        private(&mut registry, &context);
        registry.add_sources(&context, [Source::UnknownPrivate]).unwrap();
        let epoch = registry.context_epoch(&context).unwrap();
        let payload = serde_json::json!({"operation":"matrix.room.messages.search","term":"the exact reviewed query","room_ids":["room-a"]});
        let review = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), None, &payload).unwrap();
        assert!(!review.allowed); assert!(review.allow_session());
        assert!(registry.effect_authorities().unwrap().is_empty());
        registry.approve_effect_once(&review).unwrap();
        let changed = serde_json::json!({"operation":"matrix.room.messages.search","term":"a different query","room_ids":["room-a"]});
        assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), None, &changed).is_err());
        let other = Recipient::NetworkOrigin("https://other.example".into());
        assert!(registry.commit_effect_for_activation(&context, epoch, Some(&other), None, &payload).is_err());
        registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), None, &payload).unwrap();
        assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), None, &payload).is_err());
        assert!(registry.sharing_grants().unwrap().is_empty());
        assert!(registry.ensure_allowed(&context, &recipient()).is_err());
        assert!(root.registry().effect_authorities().unwrap().is_empty());
        let next = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), None, &payload).unwrap();
        registry.approve_effect_once(&next).unwrap();
        registry.remove_context(&context); registry.register_context(&context).unwrap();
        assert!(registry.approve_effect_once(&next).is_err());
        let fresh_epoch = registry.context_epoch(&context).unwrap();
        assert!(registry.commit_effect_for_activation(&context, fresh_epoch, Some(&recipient()), None, &payload).is_err());
    }

    #[test]
    fn legacy_operation_approval_reuses_the_selected_duration_without_declassifying_data() {
        for permanent in [false, true] {
            let root = TestRoot::new(); let mut registry = root.registry(); let context = context();
            private(&mut registry, &context);
            registry.add_sources(&context, [Source::UnknownPrivate]).unwrap();
            registry.add_influences(&context, [Influence::Unknown]).unwrap();
            let epoch = registry.context_epoch(&context).unwrap();
            let payload = serde_json::json!({"operation":"matrix.room.event.read","event_id":"first"});
            let review = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), None, &payload).unwrap();
            assert!(review.allow_session());
            if permanent { registry.approve_effect_always(&review).unwrap(); }
            else { registry.approve_effect_session(&review, SharingDuration::RobrixSession).unwrap(); }
            let next = serde_json::json!({"operation":"matrix.room.event.read","event_id":"next"});
            registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), None, &next).unwrap();
            assert!(registry.ensure_allowed(&context, &recipient()).is_err());
            assert!(registry.sharing_grants().unwrap().is_empty());
            assert!(registry.labels(&context).unwrap().contains(&Source::UnknownPrivate));
            assert!(registry.influences(&context).unwrap().contains(&Influence::Unknown));
            assert!(registry.commit_effect_for_activation(&context, epoch, Some(&Recipient::External), None, &next).is_err());
            assert!(registry.commit_effect_for_activation(&context, epoch,
                Some(&Recipient::NetworkOrigin("https://other.example".into())), None, &next).is_err());
            assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), None,
                &serde_json::json!({"operation":"matrix.user.profile.read","event_id":"next"})).is_err());
            let mut reopened = root.registry(); reopened.register_context(&context).unwrap();
            let reopened_epoch = reopened.context_epoch(&context).unwrap();
            assert_eq!(reopened.commit_effect_for_activation(&context, reopened_epoch, Some(&recipient()), None, &next).is_ok(), permanent);
            registry.add_sources(&context, [Source::Room { account: "alice".into(), room: "new-room".into() }]).unwrap();
            assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), None, &next).is_err());
            let grant = registry.effect_authorities().unwrap().pop().unwrap();
            registry.revoke_effect_authority(grant.id).unwrap();
            assert!(registry.effect_authorities().unwrap().is_empty());
        }
    }

    fn scoped_context(room: &str) -> ContextId {
        ContextId::App { account: "alice".into(), app: "scoped-app".into(), room: Some(room.into()) }
    }

    fn scoped_private(registry: &mut Registry, context: &ContextId) {
        private(registry, context);
        let room = context.room().unwrap();
        registry.add_sources(context, [Source::UnknownPrivate, Source::Room { account: "alice".into(), room: room.into() }]).unwrap();
        registry.add_influences(context, [Influence::Unknown, Influence::RoomContent { account: "alice".into(), room: room.into() }]).unwrap();
    }

    #[test]
    fn all_rooms_approval_reuses_only_this_apps_room_operation_and_reviewed_non_room_inputs() {
        for permanent in [false, true] {
            let root = TestRoot::new(); let mut registry = root.registry();
            let first = scoped_context("room-a"); let second = scoped_context("room-b");
            scoped_private(&mut registry, &first); scoped_private(&mut registry, &second);
            let destination = |room: &str| Recipient::MatrixRoom { account: "alice".into(), room: room.into() };
            let action = |room: &str| SensitiveAction { kind: "matrix.room.message.send".into(), target: room.into() };
            let epoch = registry.context_epoch(&first).unwrap();
            let payload = serde_json::json!({"body":"first"});
            let review = registry.prepare_effect_for_activation(&first, epoch, Some(&destination("room-a")), Some(&action("room-a")), &payload).unwrap();
            let duration = if permanent { SharingDuration::Permanent } else { SharingDuration::RobrixSession };
            registry.approve_effect_scoped(&review, RoomScope::AllRooms, duration).unwrap();
            let next_epoch = registry.context_epoch(&second).unwrap();
            registry.commit_effect_for_activation(&second, next_epoch, Some(&destination("room-b")), Some(&action("room-b")), &serde_json::json!({"body":"second"})).unwrap();
            assert!(registry.sharing_grants().unwrap().is_empty());
            assert!(registry.ensure_allowed(&second, &destination("room-a")).is_err());
            assert!(registry.commit_effect_for_activation(&second, next_epoch, Some(&destination("room-b")),
                Some(&SensitiveAction { kind: "matrix.room.invite.send".into(), target: "room-b".into() }), &payload).is_err());
            let other = ContextId::App { account: "alice".into(), app: "other-app".into(), room: Some("room-b".into()) };
            scoped_private(&mut registry, &other);
            assert!(registry.commit_effect_for_activation(&other, registry.context_epoch(&other).unwrap(), Some(&destination("room-b")), Some(&action("room-b")), &payload).is_err());
            let mut reopened = root.registry(); reopened.register_context(&second).unwrap();
            assert_eq!(reopened.commit_effect_for_activation(&second, reopened.context_epoch(&second).unwrap(), Some(&destination("room-b")), Some(&action("room-b")), &payload).is_ok(), permanent);
            registry.add_influences(&second, [Influence::InternetOrigin("https://new.example".into())]).unwrap();
            assert!(registry.commit_effect_for_activation(&second, next_epoch, Some(&destination("room-b")), Some(&action("room-b")), &payload).is_err());
            let id = registry.effect_authorities().unwrap()[0].id;
            assert!(registry.revoke_effect_authority(id).unwrap());
        }
    }

    #[test]
    fn selected_room_checks_actual_server_targets_even_when_the_floor_contains_other_rooms() {
        let root = TestRoot::new(); let mut registry = root.registry();
        let context = scoped_context("origin"); scoped_private(&mut registry, &context);
        registry.add_sources(&context, [Source::Room { account: "alice".into(), room: "selected".into() }]).unwrap();
        let epoch = registry.context_epoch(&context).unwrap();
        let query = |room: &str| serde_json::json!({"operation":"matrix.room.event.read","parameters":{"room_id":room,"event_id":"event"}});
        let review = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), None, &query("selected")).unwrap();
        registry.approve_effect_scoped(&review, RoomScope::room("selected"), SharingDuration::Permanent).unwrap();
        registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), None, &query("selected")).unwrap();
        assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), None, &query("origin")).is_err());
        let injected = serde_json::json!({"operation":"matrix.room.event.read","scope_targets":[],"parameters":{"room_id":"origin"}});
        assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), None, &injected).is_err());
        for parameters in [serde_json::json!({"room_ids":[],"room_id":"origin"}),
            serde_json::json!({"room_ids":["selected"],"rooms":["origin"]}),
            serde_json::json!({"room_id":"selected","space_id":"origin"}),
            serde_json::json!({"room_ids":[],"event_id":"event"})]
        {
            let conflicting = serde_json::json!({"operation":"matrix.room.event.read","parameters":parameters});
            assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), None, &conflicting).is_err());
        }
        let selected_context = scoped_context("selected"); scoped_private(&mut registry, &selected_context);
        registry.commit_effect_for_activation(&selected_context, registry.context_epoch(&selected_context).unwrap(), Some(&recipient()), None, &query("selected")).unwrap();
        let outside = scoped_context("outside"); scoped_private(&mut registry, &outside);
        assert!(registry.commit_effect_for_activation(&outside, registry.context_epoch(&outside).unwrap(), Some(&recipient()), None, &query("selected")).is_err());
        assert!(registry.commit_effect_for_activation(&context, epoch, Some(&Recipient::NetworkOrigin("https://other.example".into())), None, &query("selected")).is_err());
        let denied = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), None, &query("origin")).unwrap();
        assert!(registry.approve_effect_scoped(&denied, RoomScope::room("selected"), SharingDuration::Permanent).is_err());
    }

    #[test]
    fn selected_space_uses_live_host_ancestry_and_fails_closed_after_membership_changes() {
        use std::sync::atomic::AtomicBool;
        static MEMBER: AtomicBool = AtomicBool::new(false);
        fn membership(account: &str, scope: &RoomScope, room: &str) -> bool {
            account == "alice" && room == "child" && MEMBER.load(Ordering::SeqCst)
                && matches!(scope, RoomScope::Selection { spaces, .. } if spaces.iter().any(|space| space == "space"))
        }
        let root = TestRoot::new(); let mut registry = root.registry();
        let context = scoped_context("child"); scoped_private(&mut registry, &context);
        let epoch = registry.context_epoch(&context).unwrap();
        let destination = Recipient::MatrixRoom { account: "alice".into(), room: "child".into() };
        let action = SensitiveAction { kind: "matrix.room.message.send".into(), target: "child".into() };
        let payload = serde_json::json!({"body":"test"});
        let review = registry.prepare_effect_for_activation(&context, epoch, Some(&destination), Some(&action), &payload).unwrap();
        let scope = RoomScope::Selection { rooms: Vec::new(), spaces: vec!["space".into()] };
        assert!(registry.approve_effect_scoped(&review, scope.clone(), SharingDuration::Permanent).is_err());
        registry.set_effect_room_scope_matcher(membership).unwrap(); MEMBER.store(true, Ordering::SeqCst);
        registry.approve_effect_scoped(&review, scope, SharingDuration::Permanent).unwrap();
        registry.commit_effect_for_activation(&context, epoch, Some(&destination), Some(&action), &payload).unwrap();
        MEMBER.store(false, Ordering::SeqCst);
        assert!(registry.commit_effect_for_activation(&context, epoch, Some(&destination), Some(&action), &payload).is_err());
    }

    #[test]
    fn scoped_room_target_can_differ_from_origin_but_url_actions_and_legacy_grants_stay_exact() {
        let root = TestRoot::new(); let mut registry = root.registry();
        let context = scoped_context("origin"); scoped_private(&mut registry, &context);
        let epoch = registry.context_epoch(&context).unwrap();
        let destination = Recipient::MatrixRoom { account: "alice".into(), room: "target".into() };
        let action = SensitiveAction { kind: "matrix.room.message.send".into(), target: "target".into() };
        let payload = serde_json::json!({"body":"reply","scope_targets":[]});
        let review = registry.prepare_effect_for_activation(&context, epoch, Some(&destination), Some(&action), &payload).unwrap();
        assert!(registry.approve_effect_scoped(&review, RoomScope::room("origin"), SharingDuration::Permanent).is_err());
        registry.approve_effect_scoped(&review, RoomScope::room("target"), SharingDuration::Permanent).unwrap();
        registry.commit_effect_for_activation(&context, epoch, Some(&destination), Some(&action), &payload).unwrap();
        let network = SensitiveAction { kind: "network.POST".into(), target: "https://example.com".into() };
        let review = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), Some(&network), &payload).unwrap();
        registry.approve_effect_scoped(&review, RoomScope::AllRooms, SharingDuration::Permanent).unwrap();
        assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient()),
            Some(&SensitiveAction { kind: "network.POST".into(), target: "https://other.example".into() }), &payload).is_err());
        let grant = registry.effect_authorities().unwrap().into_iter().find(|grant| grant.action.as_ref() == Some(&network)).unwrap();
        let mut old = serde_json::to_value(&grant).unwrap(); old.as_object_mut().unwrap().remove("room_scope");
        let old: EffectAuthority = serde_json::from_value(old).unwrap(); assert!(old.room_scope.is_none());
        let second = scoped_context("target"); scoped_private(&mut registry, &second);
        let operation = operation_key(Some(&network), &payload).unwrap();
        assert!(!registry.effect_authority_matches(&old, &second, Some(&recipient()), Some(&network), &operation,
            &registry.labels(&second).unwrap(), &registry.influences(&second).unwrap(), &payload));
    }

    #[test]
    fn scoped_navigation_can_open_another_room_without_prior_room_contents() {
        for kind in ["host.nav.event", "host.nav.thread", "host.nav.link"] {
            let root = TestRoot::new(); let mut registry = root.registry();
            let context = scoped_context("origin"); scoped_private(&mut registry, &context);
            let epoch = registry.context_epoch(&context).unwrap();
            let action = SensitiveAction { kind: kind.into(), target: "!first:test".into() };
            let payload = serde_json::json!({"room_id":"!first:test","event_id":"$first:test"});
            let review = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), Some(&action), &payload).unwrap();
            registry.approve_effect_scoped(&review, RoomScope::AllRooms, SharingDuration::Permanent).unwrap();
            let next = SensitiveAction { kind: kind.into(), target: "!next:test".into() };
            let changed = serde_json::json!({"room_id":"!next:test","event_id":"$next:test"});
            registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), Some(&next), &changed).unwrap();
            assert!(!registry.labels(&context).unwrap().contains(&Source::Room { account: "alice".into(), room: "!next:test".into() }));
            assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient()),
                Some(&SensitiveAction { kind: kind.into(), target: "https://other.example".into() }), &changed).is_err());
            assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient()),
                Some(&SensitiveAction { kind: "host.nav.user".into(), target: "!next:test".into() }), &changed).is_err());
        }
    }

    #[test]
    fn spatial_scope_does_not_grant_new_unknown_inputs_or_other_context_variants() {
        let root = TestRoot::new(); let mut registry = root.registry();
        let first = scoped_context("room-a"); private(&mut registry, &first);
        registry.add_influences(&first, [Influence::RoomContent { account: "alice".into(), room: "room-a".into() }]).unwrap();
        let epoch = registry.context_epoch(&first).unwrap();
        let destination = Recipient::MatrixRoom { account: "alice".into(), room: "room-a".into() };
        let action = SensitiveAction { kind: "matrix.room.message.send".into(), target: "room-a".into() };
        let payload = serde_json::json!({"body":"test"});
        let review = registry.prepare_effect_for_activation(&first, epoch, Some(&destination), Some(&action), &payload).unwrap();
        registry.approve_effect_scoped(&review, RoomScope::AllRooms, SharingDuration::Permanent).unwrap();
        registry.add_sources(&first, [Source::UnknownPrivate]).unwrap();
        assert!(registry.commit_effect_for_activation(&first, epoch, Some(&destination), Some(&action), &payload).is_err());
        let no_room = ContextId::App { account: "alice".into(), app: "scoped-app".into(), room: None };
        private(&mut registry, &no_room);
        registry.add_influences(&no_room, [Influence::RoomContent { account: "alice".into(), room: "room-a".into() }]).unwrap();
        assert!(registry.commit_effect_for_activation(&no_room, registry.context_epoch(&no_room).unwrap(), Some(&destination), Some(&action), &payload).is_err());
        for context in [ContextId::PublicApp { account: "alice".into(), app: "scoped-app".into() },
            ContextId::Agent { account: "alice".into(), room: "room-a".into() }]
        {
            registry.register_context(&context).unwrap();
            registry.add_influences(&context, [Influence::RoomContent { account: "alice".into(), room: "room-a".into() }]).unwrap();
            assert!(registry.commit_effect_for_activation(&context, registry.context_epoch(&context).unwrap(), Some(&destination), Some(&action), &payload).is_err());
        }
    }

    #[test]
    fn revoked_sharing_permissions_invalidate_the_displayed_capture() {
        let root = TestRoot::new(); let mut registry = root.registry(); let context = context();
        private(&mut registry, &context); let epoch = registry.context_epoch(&context).unwrap();
        let source = Source::Account { account: "alice".into() };
        let grant = registry.grant_sharing(source, recipient(), ReaderScope::Context(context.clone()), SharingDuration::RobrixSession).unwrap();
        let payload = serde_json::json!({"body":"one"});
        let review = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), Some(&action()), &payload).unwrap();
        registry.approve_effect_once(&review).unwrap(); registry.revoke_sharing(grant).unwrap();
        assert!(registry.approve_effect_once(&review).is_err());
        assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), Some(&action()), &payload).is_err());
    }

    #[test]
    fn clipboard_approval_releases_only_the_reviewed_text_and_room_close_cancels_it() {
        let root = TestRoot::new(); let mut registry = root.registry(); let context = context();
        private(&mut registry, &context); let epoch = registry.context_epoch(&context).unwrap();
        let action = SensitiveAction { kind: "device.clipboard.write".into(), target: "clipboard".into() };
        let payload = serde_json::json!({"text":"selected private text"});
        let review = registry.prepare_effect_for_activation(&context, epoch, Some(&Recipient::Clipboard), Some(&action), &payload).unwrap();
        registry.approve_effect_once(&review).unwrap();
        assert!(registry.commit_effect_for_activation(&context, epoch, Some(&Recipient::Clipboard), Some(&action), &serde_json::json!({"text":"different private text"})).is_err());
        registry.commit_effect_for_activation(&context, epoch, Some(&Recipient::Clipboard), Some(&action), &payload).unwrap();
        assert!(registry.ensure_allowed(&context, &Recipient::Clipboard).is_err());
        let retry = registry.prepare_effect_for_activation(&context, epoch, Some(&Recipient::Clipboard), Some(&action), &payload).unwrap();
        registry.approve_effect_once(&retry).unwrap(); registry.close_room_session("alice", "room-a").unwrap();
        assert!(registry.approve_effect_once(&retry).is_err());
        assert!(registry.commit_effect_for_activation(&context, epoch, Some(&Recipient::Clipboard), Some(&action), &payload).is_err());
    }

    fn browser(args: &serde_json::Value) -> (Recipient, SensitiveAction) {
        let capability = crate::capabilities::for_service("url.open").unwrap();
        let contract = capability.flow_contract().unwrap();
        (contract.recipient("alice", None, args, None).unwrap().unwrap(),
            contract.sensitive_action(capability.id, args, None).unwrap())
    }

    #[test]
    fn browser_once_approval_captures_the_full_url_and_cannot_open_changed_links() {
        let root = TestRoot::new(); let mut registry = root.registry(); let context = context();
        private(&mut registry, &context); let epoch = registry.context_epoch(&context).unwrap();
        let payload = serde_json::json!({"url":"https://example.com/manage?token=private#device"});
        let (recipient, action) = browser(&payload);
        let review = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient), Some(&action), &payload).unwrap();
        assert!(!review.allowed); assert_eq!(review.denied_sources.len(), 2);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&review.payload).unwrap(), payload);
        registry.approve_effect_once(&review).unwrap();
        for url in ["https://example.com/changed?token=private#device", "https://other.example/manage?token=private#device"] {
            let changed = serde_json::json!({"url":url}); let (recipient, action) = browser(&changed);
            assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient), Some(&action), &changed).is_err());
        }
        registry.commit_effect_for_activation(&context, epoch, Some(&recipient), Some(&action), &payload).unwrap();
        assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient), Some(&action), &payload).is_err());
        assert!(registry.sharing_grants().unwrap().is_empty()); assert!(registry.authorities().unwrap().is_empty());
    }

    #[test]
    fn public_browser_session_authority_does_not_cover_other_origins() {
        let root = TestRoot::new(); let mut registry = root.registry(); let context = context();
        registry.register_context(&context).unwrap();
        registry.add_influences(&context, [Influence::Unknown]).unwrap();
        let epoch = registry.context_epoch(&context).unwrap();
        let payload = serde_json::json!({"url":"https://Example.COM:443/first"});
        let (recipient, action) = browser(&payload);
        let review = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient), Some(&action), &payload).unwrap();
        assert!(!review.allowed); assert!(review.sources.is_empty()); assert!(review.denied_sources.is_empty());
        registry.approve_effect_session(&review, SharingDuration::RobrixSession).unwrap();
        let same_origin = serde_json::json!({"url":"https://example.com/second?query=public"});
        let (same_recipient, same_action) = browser(&same_origin);
        registry.commit_effect_for_activation(&context, epoch, Some(&same_recipient), Some(&same_action), &same_origin).unwrap();
        for url in ["https://other.example/first", "http://example.com/first", "https://example.com:8443/first"] {
            let changed = serde_json::json!({"url":url});
            let (recipient, action) = browser(&changed);
            let review = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient), Some(&action), &changed).unwrap();
            assert!(review.denied_sources.is_empty(), "public contents need no sharing grant");
            assert!(!review.allowed, "action authority must require review for another browser origin");
        }
        assert!(registry.sharing_grants().unwrap().is_empty(), "this regression must exercise integrity independently of private sharing");
    }

    #[test]
    fn browser_session_choice_covers_only_its_origin_action_and_reviewed_influences() {
        let root = TestRoot::new(); let mut registry = root.registry(); let context = context();
        private(&mut registry, &context); let epoch = registry.context_epoch(&context).unwrap();
        let payload = serde_json::json!({"url":"https://Example.COM:443/manage?token=private"});
        let (recipient, action) = browser(&payload);
        let review = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient), Some(&action), &payload).unwrap();
        registry.approve_effect_session(&review, SharingDuration::RobrixSession).unwrap();
        let changed = serde_json::json!({"url":"https://example.com/devices?device=two"});
        let (same_recipient, same_action) = browser(&changed);
        assert_eq!(recipient, same_recipient); assert_eq!(action, same_action);
        registry.commit_effect_for_activation(&context, epoch, Some(&same_recipient), Some(&same_action), &changed).unwrap();
        for url in ["http://example.com/manage", "https://example.com:8443/manage", "https://other.example/manage"] {
            let changed = serde_json::json!({"url":url}); let (recipient, action) = browser(&changed);
            let review = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient), Some(&action), &changed).unwrap();
            assert!(!review.allowed); assert_eq!(review.denied_sources.len(), 2);
        }
        let post = SensitiveAction { kind: "network.POST".into(), target: "https://example.com".into() };
        assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient), Some(&post), &changed).is_err());
        registry.add_influences(&context, [Influence::InternetOrigin("https://new.example".into())]).unwrap();
        let fresh = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient), Some(&action), &payload).unwrap();
        assert!(!fresh.allowed); assert_eq!(fresh.denied_sources.len(), 2);
        registry.end_session().unwrap(); registry.register_context(&context).unwrap();
        assert!(registry.ensure_allowed(&context, &recipient).is_err());
    }

    #[test]
    fn non_http_links_refuse_private_data_and_public_action_choices_keep_exact_targets() {
        let root = TestRoot::new(); let mut registry = root.registry(); let context = context();
        registry.register_context(&context).unwrap();
        registry.add_influences(&context, [Influence::Unknown]).unwrap();
        let epoch = registry.context_epoch(&context).unwrap();
        let payload = serde_json::json!({"url":"mailto:alice@example.com?subject=Hello"});
        let (recipient, action) = browser(&payload);
        assert_eq!(recipient, Recipient::External);
        let review = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient), Some(&action), &payload).unwrap();
        assert!(!review.allowed); assert!(review.denied_sources.is_empty());
        registry.approve_effect_session(&review, SharingDuration::RobrixSession).unwrap();
        registry.commit_effect_for_activation(&context, epoch, Some(&recipient), Some(&action), &payload).unwrap();
        let changed = serde_json::json!({"url":"mailto:bob@example.com?subject=Hello"});
        let (other_recipient, other_action) = browser(&changed);
        assert_eq!(recipient, other_recipient); assert_ne!(action, other_action);
        assert!(registry.commit_effect_for_activation(&context, epoch, Some(&other_recipient), Some(&other_action), &changed).is_err());
        registry.add_sources(&context, [Source::Account { account: "alice".into() }]).unwrap();
        for payload in [payload, changed, serde_json::json!({"url":"file:///private/data"})] {
            let (recipient, action) = browser(&payload);
            assert!(registry.prepare_effect_for_activation(&context, epoch, Some(&recipient), Some(&action), &payload).is_err());
        }
    }

    #[test]
    fn foreign_source_permission_is_required_without_blocking_already_authorized_data() {
        let root = TestRoot::new(); let mut registry = root.registry(); let context = context();
        registry.register_context(&context).unwrap(); let epoch = registry.context_epoch(&context).unwrap();
        let foreign = Source::Room { account: "bob".into(), room: "room-b".into() };
        registry.add_sources(&context, [foreign.clone()]).unwrap();
        let payload = serde_json::json!({"body":"one"});
        let blocked = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), Some(&action()), &payload).unwrap();
        assert!(registry.approve_effect_once(&blocked).is_err());
        assert!(registry.approve_effect_session(&blocked, SharingDuration::RobrixSession).is_err());
        registry.grant_sharing(foreign, recipient(), ReaderScope::AllReaders, SharingDuration::Permanent).unwrap();
        let reviewed = registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), Some(&action()), &payload).unwrap();
        assert!(reviewed.denied_sources.is_empty()); assert!(!reviewed.allowed);
        assert!(!reviewed.allow_session());
        assert!(registry.approve_effect_always(&reviewed).is_err());
        registry.approve_effect_once(&reviewed).unwrap();
        registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), Some(&action()), &payload).unwrap();
    }
}
