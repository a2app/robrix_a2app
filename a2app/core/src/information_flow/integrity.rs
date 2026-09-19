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
}

impl Registry {
    pub fn action_decision(&self, context: &ContextId, action: &SensitiveAction) -> Result<ActionDecision, String> {
        validate_action(action)?;
        let influences = self.influences(context)?;
        let allowed = influences.is_empty() || self.authorities.iter().any(|grant|
            &grant.context == context && &grant.action == action && influences.is_subset(&grant.influences));
        let decision = ActionDecision { context: context.clone(), epoch: self.context_epoch(context)?, action: action.clone(), influences, allowed };
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
    if action.kind.is_empty() || action.target.is_empty() { return Err("An exact action kind and target are required.".into()); }
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
