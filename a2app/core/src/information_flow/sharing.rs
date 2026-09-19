use super::*;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ReaderScope {
    AllReaders,
    App { account: String, app: String },
    Context(ContextId),
}

impl ReaderScope {
    fn matches(&self, context: Option<&ContextId>) -> bool {
        match self {
            Self::AllReaders => true,
            Self::App { account, app } => context.is_some_and(|context| context.account() == account && context.app() == Some(app.as_str())),
            Self::Context(expected) => context == Some(expected),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SharingDuration {
    Permanent,
    RobrixSession,
    RoomSession { account: String, room: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharingGrant {
    pub id: u64,
    pub source: Source,
    pub recipient: Recipient,
    pub reader: ReaderScope,
    pub duration: SharingDuration,
}

/// Trusted UI metadata only. Never forward source identities into an app.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlowDecision {
    pub context: ContextId,
    pub epoch: u64,
    pub recipient: Recipient,
    pub sources: Label,
    pub denied_sources: Label,
    pub allowed: bool,
}

impl Registry {
    fn source_allowed(&self, source: &Source, recipient: &Recipient, context: Option<&ContextId>) -> bool {
        if *source == Source::UnknownPrivate { return false; }
        if matches!((source, recipient),
            (Source::Room { account, room }, Recipient::MatrixRoom { account: target_account, room: target_room })
            if account == target_account && room == target_room)
        { return true; }
        self.metadata.grants.iter().chain(self.session_grants.iter()).any(|grant|
            &grant.source == source && &grant.recipient == recipient && grant.reader.matches(context))
    }

    pub fn decision(&self, context: &ContextId, recipient: &Recipient) -> Result<FlowDecision, String> {
        let sources = self.labels(context)?;
        recipient.validate()?;
        let denied_sources: Label = sources.iter().filter(|source| !self.source_allowed(source, recipient, Some(context))).cloned().collect();
        let decision = FlowDecision { context: context.clone(), epoch: self.context_epoch(context)?, recipient: recipient.clone(), sources,
            allowed: denied_sources.is_empty(), denied_sources };
        let mut recent = self.decisions.borrow_mut();
        if recent.back() != Some(&decision) {
            if recent.len() == 256 { recent.pop_front(); }
            recent.push_back(decision.clone());
        }
        Ok(decision)
    }

    pub fn recent_decisions(&self) -> Result<Vec<FlowDecision>, String> {
        self.check_healthy()?;
        Ok(self.decisions.borrow().iter().cloned().collect())
    }

    pub fn ensure_allowed(&self, context: &ContextId, recipient: &Recipient) -> Result<(), String> {
        let decision = self.decision(context, recipient)?;
        if decision.allowed { Ok(()) } else { Err(denied_message(&decision.denied_sources, recipient)) }
    }

    /// Without an identified reader only AllReaders grants can authorize data.
    pub fn ensure_labels_allowed(&self, label: &Label, recipient: &Recipient) -> Result<(), String> {
        self.check_healthy()?;
        recipient.validate()?;
        for source in label { validate_source(source)?; }
        let denied: Label = label.iter().filter(|source| !self.source_allowed(source, recipient, None)).cloned().collect();
        if denied.is_empty() { Ok(()) } else { Err(denied_message(&denied, recipient)) }
    }

    pub fn grant_sharing(&mut self, source: Source, recipient: Recipient, reader: ReaderScope, duration: SharingDuration) -> Result<u64, String> {
        self.check_healthy()?;
        let mut grant = SharingGrant { id: 0, source, recipient, reader, duration };
        validate_grant(&grant)?;
        if let Some(existing) = self.metadata.grants.iter().chain(self.session_grants.iter()).find(|existing|
            existing.source == grant.source && existing.recipient == grant.recipient && existing.reader == grant.reader && existing.duration == grant.duration)
        { return Ok(existing.id); }
        if grant.duration == SharingDuration::Permanent {
            let mut next = self.metadata.clone();
            grant.id = storage::next_grant_id(&mut next)?;
            let id = grant.id;
            next.grants.push(grant);
            self.persist(next)?;
            Ok(id)
        } else {
            grant.id = self.ephemeral_id()?;
            let id = grant.id;
            self.session_grants.push(grant);
            Ok(id)
        }
    }

    pub fn revoke_sharing(&mut self, id: u64) -> Result<bool, String> {
        self.check_healthy()?;
        if self.session_grants.iter().any(|grant| grant.id == id) {
            self.session_grants.retain(|grant| grant.id != id);
            return Ok(true);
        }
        if !self.metadata.grants.iter().any(|grant| grant.id == id) { return Ok(false); }
        let mut next = self.metadata.clone();
        next.grants.retain(|grant| grant.id != id);
        self.persist(next)?;
        Ok(true)
    }

    pub fn sharing_grants(&self) -> Result<Vec<SharingGrant>, String> {
        self.check_healthy()?;
        Ok(self.metadata.grants.iter().chain(self.session_grants.iter()).cloned().collect())
    }

    /// Compatibility: a legacy policy changes only permanent AllReaders grants.
    pub fn set_policy(&mut self, source: Source, policy: FlowPolicy) -> Result<(), String> {
        self.check_healthy()?;
        validate_source(&source)?;
        let mut next = self.metadata.clone();
        next.grants.retain(|grant| grant.source != source || grant.reader != ReaderScope::AllReaders);
        for recipient in policy.recipients {
            let grant = SharingGrant { id: storage::next_grant_id(&mut next)?, source: source.clone(), recipient,
                reader: ReaderScope::AllReaders, duration: SharingDuration::Permanent };
            validate_grant(&grant)?;
            next.grants.push(grant);
        }
        self.persist(next)
    }

    pub fn policies(&self) -> Result<BTreeMap<Source, FlowPolicy>, String> {
        self.check_healthy()?;
        let mut policies = BTreeMap::<Source, FlowPolicy>::new();
        for grant in self.metadata.grants.iter().filter(|grant| grant.reader == ReaderScope::AllReaders) {
            policies.entry(grant.source.clone()).or_default().recipients.insert(grant.recipient.clone());
        }
        Ok(policies)
    }

    pub fn policy_for(&self, source: &Source) -> Result<FlowPolicy, String> {
        validate_source(source)?;
        Ok(self.policies()?.remove(source).unwrap_or_default())
    }
}

fn denied_message(denied: &Label, recipient: &Recipient) -> String {
    if denied.contains(&Source::UnknownPrivate) { "Stored data with unknown sources cannot be shared." }
    else if *recipient == Recipient::External { "This action cannot safely export private data yet." }
    else { "Information flow blocked: this context has private data that is not allowed to reach this recipient. Review the blocked flow in Mini Apps." }.into()
}

pub(super) fn validate_grant(grant: &SharingGrant) -> Result<(), String> {
    validate_source(&grant.source)?;
    if grant.source == Source::UnknownPrivate {
        return Err("Storage with unknown private sources cannot be released through a sharing rule.".into());
    }
    grant.recipient.validate()?;
    match &grant.reader {
        ReaderScope::AllReaders => {}
        ReaderScope::App { account, app } => validate_context(&ContextId::PublicApp { account: account.clone(), app: app.clone() })?,
        ReaderScope::Context(context) => validate_context(context)?,
    }
    if let SharingDuration::RoomSession { account, room } = &grant.duration {
        validate_context(&ContextId::Agent { account: account.clone(), room: room.clone() })?;
    }
    Ok(())
}
