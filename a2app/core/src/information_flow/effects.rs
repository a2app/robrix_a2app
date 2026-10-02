//! Combined, host-captured sharing and action approval for one final effect.

use super::*;

const MAX_PENDING_EFFECTS: usize = 64;
pub const EFFECT_REVIEW_REQUIRED: &str = "Your approval is needed before this request can continue.";

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
    fn same_capture(&self, other: &Self) -> bool {
        self.id == other.id && self.context == other.context && self.epoch == other.epoch
            && self.recipient == other.recipient && self.action == other.action
            && self.sources == other.sources && self.denied_sources == other.denied_sources
            && self.influences == other.influences && self.payload == other.payload
    }
}

impl Registry {
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
        if denied_sources.is_empty() && action_allowed {
            self.pending_effects.retain(|pending| pending.review.context != *context || pending.review.epoch != epoch
                || pending.review.recipient.as_ref() != recipient || pending.review.action.as_ref() != action);
            return Ok(EffectReview { id: 0, context: context.clone(), epoch, recipient: recipient.cloned(), action: action.cloned(),
                sources, denied_sources, influences, payload: "".into(), allowed: true });
        }
        if sources.contains(&Source::UnknownPrivate) { return Err("This app has older private data whose source is unknown. It cannot be shared through a permission prompt.".into()); }
        if recipient == Some(&Recipient::External) { return Err("This request has no specific destination and cannot share private data through a permission prompt.".into()); }
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
            Source::Account { account } | Source::Room { account, .. } => account != expected.context.account(),
            Source::UnknownPrivate => true,
        }) { return Err("This request contains private data that cannot be approved from the current account.".into()); }
        Ok(())
    }

    /// One immutable operation only; creates no source or session sharing rule.
    pub fn approve_effect_once(&mut self, expected: &EffectReview) -> Result<(), String> {
        self.reviewed_effect(expected)?;
        self.pending_effects.iter_mut().find(|pending| pending.review.id == expected.id).unwrap().approved_once = true;
        Ok(())
    }

    /// Approve all known sources and this operation/target in one transaction.
    ///
    /// The UI must explain that session permission also covers other contents.
    /// Future additional influences still require a new integrity review.
    pub fn approve_effect_session(&mut self, expected: &EffectReview, duration: SharingDuration) -> Result<(), String> {
        self.reviewed_effect(expected)?;
        let session = match &duration {
            SharingDuration::RobrixSession => AuthoritySession::RobrixSession,
            SharingDuration::RoomSession { account, room } if account == expected.context.account() && expected.context.room() == Some(room.as_str()) => {
                AuthoritySession::RoomSession { account: account.clone(), room: room.clone() }
            }
            _ => return Err("This request can only be approved for this Robrix session or its own room session.".into()),
        };
        let denied_sources = if let Some(recipient) = &expected.recipient {
            self.decision(&expected.context, recipient)?.denied_sources
        } else { Label::new() };
        let mut sharing = Vec::new();
        for source in denied_sources {
            let grant = SharingGrant { id: 0, source, recipient: expected.recipient.clone().unwrap(), reader: ReaderScope::Context(expected.context.clone()), duration: duration.clone() };
            sharing::validate_grant(&grant)?;
            sharing.push(grant);
        }
        let action = expected.action.as_ref().filter(|action| !expected.influences.is_empty()
            && !self.authorities.iter().any(|grant| grant.context == expected.context && &grant.action == *action
                && grant.session == session && expected.influences.is_subset(&grant.influences))).cloned();
        let count = sharing.len() + usize::from(action.is_some());
        let next = self.next_ephemeral_id.checked_add(count as u64).ok_or("Information-flow grant identities exhausted.")?;
        let mut id = self.next_ephemeral_id;
        for grant in &mut sharing { grant.id = id; id += 1; }
        if let Some(action) = action {
            self.authorities.push(ActionAuthority { id, context: expected.context.clone(), action, session, influences: expected.influences.clone() });
        }
        self.session_grants.extend(sharing);
        self.next_ephemeral_id = next;
        self.pending_effects.retain(|pending| pending.review.id != expected.id);
        Ok(())
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
        assert_eq!(registry.sharing_grants().unwrap().len(), 2); assert_eq!(registry.authorities().unwrap().len(), 1);
        registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), Some(&action()), &serde_json::json!({"body":"two"})).unwrap();
        registry.add_influences(&context, [Influence::Unknown]).unwrap();
        assert!(registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), Some(&action()), &payload).is_err());
        registry.end_session().unwrap(); registry.register_context(&context).unwrap();
        assert!(registry.ensure_allowed(&context, &recipient()).is_err());
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
            assert_eq!(registry.authorities().unwrap().is_empty(), duration != SharingDuration::RobrixSession);
            registry.end_session().unwrap();
            assert!(registry.authorities().unwrap().is_empty());
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
        assert!(registry.prepare_effect_for_activation(&context, epoch, Some(&recipient()), None, &payload).is_err());
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
        assert!(registry.prepare_effect_for_activation(&context, epoch, None, Some(&action()), &payload).is_err());
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
        registry.approve_effect_once(&reviewed).unwrap();
        registry.commit_effect_for_activation(&context, epoch, Some(&recipient()), Some(&action()), &payload).unwrap();
    }
}
