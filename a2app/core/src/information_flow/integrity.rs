use super::*;

/// Host-attributed untrusted influence. This is independent of confidentiality.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum Influence {
    RoomContent { account: String, room: String },
    InternetOrigin(String),
    MiniApp { account: String, app: String },
    Model(String),
    Unknown,
}

pub type Influences = BTreeSet<Influence>;

/// Exact host-defined operation and destination; neither field is a wildcard.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SensitiveAction {
    pub kind: String,
    pub target: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthoritySession {
    /// One host-captured request; never grants operation-wide authority.
    Once { request_id: u64 },
    RobrixSession,
    RoomSession { account: String, room: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionAuthority {
    pub id: u64,
    pub context: ContextId,
    pub action: SensitiveAction,
    pub session: AuthoritySession,
    /// New influences after review require another user authorization.
    pub influences: Influences,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionDecision {
    pub context: ContextId,
    pub epoch: u64,
    pub action: SensitiveAction,
    pub influences: Influences,
    pub allowed: bool,
    /// Exact host-captured arguments. Kept only in bounded process memory.
    pub request: Option<ActionRequest>,
}

impl Registry {
    pub fn action_decision(&self, context: &ContextId, action: &SensitiveAction) -> Result<ActionDecision, String> {
        validate_action(action)?;
        let influences = self.influences(context)?;
        let allowed = influences.is_empty() || self.authorities.iter().any(|grant|
            &grant.context == context && &grant.action == action && influences.is_subset(&grant.influences)
                && !matches!(grant.session, AuthoritySession::Once { .. }));
        let decision = ActionDecision { context: context.clone(), epoch: self.context_epoch(context)?, action: action.clone(), influences, allowed, request: None };
        let mut recent = self.action_decisions.borrow_mut();
        if recent.back() != Some(&decision) {
            if recent.len() == 256 { recent.pop_front(); }
            recent.push_back(decision.clone());
        }
        Ok(decision)
    }

    pub fn ensure_action_allowed(&self, context: &ContextId, action: &SensitiveAction) -> Result<(), String> {
        if self.action_decision(context, action)?.allowed { Ok(()) }
        else { Err("Untrusted input influenced this action. Review its exact operation and target in Mini Apps before authorizing it.".into()) }
    }

    pub fn recent_action_decisions(&self) -> Result<Vec<ActionDecision>, String> {
        self.check_healthy()?;
        Ok(self.action_decisions.borrow().iter().cloned().collect())
    }

    /// Trusted callers only. UI must use the checked form with what it showed.
    pub fn grant_authority(&mut self, context: &ContextId, action: SensitiveAction, session: AuthoritySession) -> Result<u64, String> {
        let influences = self.influences(context)?;
        self.grant_authority_checked(context, action, session, &influences)
    }

    pub fn grant_authority_checked(&mut self, context: &ContextId, action: SensitiveAction, session: AuthoritySession, expected: &Influences) -> Result<u64, String> {
        validate_action(&action)?;
        if matches!(session, AuthoritySession::Once { .. }) { return Err("Exact-action permission requires a pending host request.".into()); }
        let influences = self.influences(context)?;
        if &influences != expected { return Err("The context received new input after this action was reviewed. Review its influences again.".into()); }
        if let AuthoritySession::RoomSession { account, room } = &session {
            if context.account() != account || context.room() != Some(room.as_str()) {
                return Err("Action authority must expire with this context's own room session.".into());
            }
        }
        if let Some(grant) = self.authorities.iter().find(|grant|
            &grant.context == context && grant.action == action && grant.session == session && grant.influences == influences)
        { return Ok(grant.id); }
        let id = self.ephemeral_id()?;
        self.authorities.push(ActionAuthority { id, context: context.clone(), action, session, influences });
        Ok(id)
    }

    /// Bind trusted user review to both the activation and displayed inputs.
    pub fn grant_authority_for_activation(&mut self, context: &ContextId, action: SensitiveAction, session: AuthoritySession, expected: &Influences, expected_epoch: u64) -> Result<u64, String> {
        self.ensure_context_epoch(context, expected_epoch)?;
        self.grant_authority_checked(context, action, session, expected)
    }

    pub fn authorities(&self) -> Result<Vec<ActionAuthority>, String> {
        self.check_healthy()?;
        Ok(self.authorities.clone())
    }

    pub fn revoke_authority(&mut self, id: u64) -> Result<bool, String> {
        self.check_healthy()?;
        let before = self.authorities.len();
        self.authorities.retain(|grant| grant.id != id);
        Ok(before != self.authorities.len())
    }
}

fn validate_action(action: &SensitiveAction) -> Result<(), String> {
    if action.kind.is_empty() || action.target.is_empty() || action.kind.len() > 256 || action.target.len() > 8192 { return Err("An exact action kind and target are required.".into()); }
    Ok(())
}

pub(super) fn validate_influence(influence: &Influence) -> Result<(), String> {
    match influence {
        Influence::RoomContent { account, room } => validate_source(&Source::Room { account: account.clone(), room: room.clone() }),
        Influence::MiniApp { account, app } => validate_context(&ContextId::PublicApp { account: account.clone(), app: app.clone() }),
        Influence::InternetOrigin(origin) => Recipient::NetworkOrigin(origin.clone()).validate(),
        Influence::Model(model) if model.is_empty() => Err("A model influence identity is required.".into()),
        _ => Ok(()),
    }
}


/// Canonical arguments shown by trusted UI, never by an app-provided summary.
#[derive(Clone, Eq, PartialEq)]
pub struct ActionRequest {
    pub id: u64,
    pub payload: std::sync::Arc<str>,
}

impl std::fmt::Debug for ActionRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActionRequest").field("id", &self.id).finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub(super) struct PendingAction {
    decision: ActionDecision,
}

const MAX_EXACT_PAYLOAD: usize = 64 * 1024;
const MAX_PENDING_ACTIONS: usize = 64;

pub const ACTION_REVIEW_REQUIRED: &str = "Untrusted input influenced this action. Open Mini Apps > Data sharing and action review, select this blocked action, review its exact contents, and choose Allow this exact action once. Then retry the unchanged action. Session permission is a separate, broader choice.";

/// Bound allocation and nesting before sorting/serializing untrusted JSON.
/// Object order has no meaning; arrays, numbers and strings retain their value.
fn canonical_payload(value: &serde_json::Value) -> Result<String, String> {
    fn measure(value: &serde_json::Value, depth: usize, remaining: &mut usize) -> Result<(), String> {
        if depth > 32 { return Err("The action is too deeply nested to review safely.".into()); }
        let size = match value {
            serde_json::Value::String(s) => s.len().checked_add(2),
            serde_json::Value::Array(a) => {
                for value in a { measure(value, depth + 1, remaining)?; }
                a.len().checked_add(2)
            }
            serde_json::Value::Object(o) => {
                for (key, value) in o {
                    *remaining = remaining.checked_sub(key.len().checked_add(2).ok_or("The action is too large to review safely.")?)
                        .ok_or("The action is too large to review safely.")?;
                    measure(value, depth + 1, remaining)?;
                }
                o.len().checked_mul(2).and_then(|s| s.checked_add(2))
            }
            _ => Some(32),
        }.ok_or("The action is too large to review safely.")?;
        *remaining = remaining.checked_sub(size).ok_or("The action is too large to review safely; reduce its contents and try again.")?;
        Ok(())
    }
    fn sorted(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => {
                let ordered: BTreeMap<_, _> = map.iter().map(|(k, v)| (k.clone(), sorted(v))).collect();
                serde_json::Value::Object(ordered.into_iter().collect())
            }
            serde_json::Value::Array(array) => serde_json::Value::Array(array.iter().map(sorted).collect()),
            _ => value.clone(),
        }
    }
    let mut remaining = MAX_EXACT_PAYLOAD;
    measure(value, 0, &mut remaining)?;
    struct Bounded(Vec<u8>);
    impl std::io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > MAX_EXACT_PAYLOAD - self.0.len() {
                return Err(std::io::Error::other("Exact action review limit exceeded"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    let mut output = Bounded(Vec::new());
    serde_json::to_writer(&mut output, &sorted(value)).map_err(|_| "The action is too large to review safely; reduce its contents and try again.")?;
    String::from_utf8(output.0).map_err(|_| "The action cannot be reviewed safely.".into())
}

impl Registry {
    /// Check a complete immutable host request without consuming approval.
    pub fn check_exact_action_for_activation(&mut self, context: &ContextId, epoch: u64, action: &SensitiveAction, payload: &serde_json::Value) -> Result<(), String> {
        self.exact_action(context, epoch, action, payload, false)
    }

    /// Consume approval immediately before starting the effect, under the
    /// registry lock. Failure/cancellation after this point never refunds it.
    pub fn commit_exact_action_for_activation(&mut self, context: &ContextId, epoch: u64, action: &SensitiveAction, payload: &serde_json::Value) -> Result<(), String> {
        self.exact_action(context, epoch, action, payload, true)
    }

    fn exact_action(&mut self, context: &ContextId, epoch: u64, action: &SensitiveAction, payload: &serde_json::Value, commit: bool) -> Result<(), String> {
        self.ensure_context_epoch(context, epoch)?;
        validate_action(action)?;
        let influences = self.influences(context)?;
        let session_allowed = influences.is_empty() || self.authorities.iter().any(|grant|
            &grant.context == context && &grant.action == action && influences.is_subset(&grant.influences)
                && !matches!(grant.session, AuthoritySession::Once { .. }));
        if session_allowed {
            // Ordinary operation limits still apply at each host service. An
            // explicitly broader session grant needs no per-payload capture.
            if commit {
                self.pending_actions.retain(|pending| &pending.decision.context != context || &pending.decision.action != action);
                self.authorities.retain(|grant| &grant.context != context || &grant.action != action
                    || !matches!(grant.session, AuthoritySession::Once { .. }));
            }
            return self.ensure_action_allowed(context, action);
        }
        let payload = match canonical_payload(payload) {
            Ok(payload) => payload,
            Err(error) => {
                // Keep an operation-only denial so the UI can still offer an
                // explicitly broader session choice. It cannot approve once.
                self.action_decision(context, action)?;
                return Err(format!("{error} No one-time permission was created. Reduce the contents, or explicitly allow this operation and target for a session in Mini Apps > Data sharing and action review."));
            }
        };
        let existing = self.pending_actions.iter().find(|pending| {
            let d = &pending.decision;
            &d.context == context && d.epoch == epoch && &d.action == action && d.influences == influences
                && d.request.as_ref().is_some_and(|request| request.payload.as_ref() == payload)
        }).map(|pending| pending.decision.request.as_ref().unwrap().id);
        let id = match existing { Some(id) => id, None => self.ephemeral_id()? };
        let once_allowed = self.authorities.iter().any(|grant| grant.context == *context
            && grant.session == AuthoritySession::Once { request_id: id } && grant.influences == influences);
        let decision = ActionDecision { context: context.clone(), epoch, action: action.clone(), influences,
            allowed: session_allowed || once_allowed, request: Some(ActionRequest { id, payload: payload.into() }) };
        if !session_allowed && existing.is_none() {
            if self.pending_actions.len() == MAX_PENDING_ACTIONS {
                if let Some(old) = self.pending_actions.pop_front() {
                    let id = old.decision.request.unwrap().id;
                    self.authorities.retain(|grant| grant.session != AuthoritySession::Once { request_id: id });
                }
            }
            self.pending_actions.push_back(PendingAction { decision: decision.clone() });
        }
        let mut recent = self.action_decisions.borrow_mut();
        // Each exact request has one current status; repeated polling neither
        // consumes its permission nor fills the bounded history.
        recent.retain(|old| old.request.as_ref().is_none_or(|request| request.id != id));
        if recent.len() == 256 { recent.pop_front(); }
        recent.push_back(decision.clone());
        drop(recent);
        if decision.allowed {
            if commit {
                self.authorities.retain(|grant| grant.session != AuthoritySession::Once { request_id: id });
                self.pending_actions.retain(|pending| pending.decision.request.as_ref().unwrap().id != id);
            }
            Ok(())
        } else {
            Err(ACTION_REVIEW_REQUIRED.into())
        }
    }

    /// Approval refers only to a still-pending host capture, never UI-supplied
    /// arguments. The UI must show its complete payload without truncation.
    pub fn grant_exact_action_for_activation(&mut self, context: &ContextId, request_id: u64, expected: &Influences, epoch: u64) -> Result<u64, String> {
        self.ensure_context_epoch(context, epoch)?;
        let influences = self.influences(context)?;
        if &influences != expected { return Err("The context received new input. Review the exact action again.".into()); }
        let pending = self.pending_actions.iter().find(|pending| {
            let d = &pending.decision;
            &d.context == context && d.epoch == epoch && &d.influences == expected
                && d.request.as_ref().is_some_and(|request| request.id == request_id)
        }).ok_or("This exact action expired, was cancelled, or was already consumed. Retry it to obtain a new review.")?;
        let session = AuthoritySession::Once { request_id };
        if let Some(grant) = self.authorities.iter().find(|grant| grant.session == session) { return Ok(grant.id); }
        let action = pending.decision.action.clone();
        let id = self.ephemeral_id()?;
        self.authorities.push(ActionAuthority { id, context: context.clone(), action, session, influences });
        Ok(id)
    }

    pub fn cancel_exact_action(&mut self, request_id: u64) -> Result<bool, String> {
        self.check_healthy()?;
        let before = self.pending_actions.len();
        self.pending_actions.retain(|pending| pending.decision.request.as_ref().unwrap().id != request_id);
        self.authorities.retain(|grant| grant.session != AuthoritySession::Once { request_id });
        Ok(before != self.pending_actions.len())
    }

    pub(super) fn close_exact_room(&mut self, account: &str, room: &str) {
        self.pending_actions.retain(|pending| pending.decision.context.account() != account
            || pending.decision.context.room() != Some(room));
        self.authorities.retain(|grant| !matches!(grant.session, AuthoritySession::Once { .. })
            || grant.context.account() != account || grant.context.room() != Some(room));
        self.action_decisions.borrow_mut().retain(|decision| decision.context.account() != account
            || decision.context.room() != Some(room));
    }

    pub(super) fn forget_exact_actions(&mut self, context: &ContextId) {
        self.pending_actions.retain(|pending| &pending.decision.context != context);
        self.action_decisions.borrow_mut().retain(|decision| &decision.context != context);
    }
}
