//! Coarse, floating information-flow labels for untrusted app and agent contexts.
//!
//! A host joins sources before delivering private data. Labels only grow, and
//! every source must permit a recipient before the host releases any output.
//! This applies to the whole context, including encoded or model-derived output;
//! it does not depend on inspecting request text. There is no declassification.
//!
//! Runtime storage is compartmentalized by account, app and room. Source-code
//! provenance remains an inherited app-wide floor; retired shared-storage files
//! are never mounted in a new compartment. Labels and integrity influences
//! persist outside the jail before input is delivered.

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    sync::{Mutex, OnceLock, atomic::{AtomicU64, Ordering}},
};

use serde::{Deserialize, Serialize};

const METADATA_FILE: &str = "information_flow.json";
const SCHEMA_VERSION: u32 = 2;

mod sharing;
mod integrity;
mod storage;
mod effects;
pub use sharing::{ReaderScope, SharingDuration, SharingGrant, FlowDecision};
pub use integrity::{Influence, Influences, SensitiveAction, AuthoritySession, ActionAuthority, ActionDecision, ActionRequest, ACTION_REVIEW_REQUIRED};
pub use effects::{EffectAuthority, EffectReview, EffectRoomScopeMatcher, EFFECT_REVIEW_REQUIRED};
use storage::{Metadata, StoredContext, StoredProvenance};

/// A private source identified by the host, never by an app-supplied argument.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum Source {
    Account { account: String },
    Room { account: String, room: String },
    /// The account's room and space directory (names, ids, counts) as
    /// returned by the AI agent's default `list_rooms` / `list_spaces` /
    /// `space_info` / `list_space_rooms` tools. Directory results carry no
    /// message content, but they are account data written by other people,
    /// so they remain untrusted and need a source of their own: one provider
    /// rule covers the whole directory and other recipients do not inherit
    /// every listed room.
    RoomDirectory { account: String },
    /// Existing private storage whose sources cannot be recovered safely.
    UnknownPrivate,
}

pub type Label = BTreeSet<Source>;

/// Exact recipients. Ordinary capability permission is additionally required.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum Recipient {
    /// Canonical HTTP(S) origin: scheme, hostname, and effective port.
    NetworkOrigin(String),
    /// Host-defined identity of the actual model endpoint and credentials.
    ModelProvider(String),
    MatrixRoom { account: String, room: String },
    /// An uncontrolled recipient, such as an exported file or a non-HTTP link.
    External,
    /// The local system clipboard; other applications may read copied text.
    Clipboard,
}

impl Recipient {
    pub fn network_origin(url: &str) -> Result<Self, String> {
        let url = url::Url::parse(url).map_err(|_| "Invalid network destination.".to_owned())?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none()
            || !url.username().is_empty() || url.password().is_some()
        {
            return Err("Network destinations must be HTTP(S) URLs without credentials.".into());
        }
        Ok(Self::NetworkOrigin(url.origin().ascii_serialization()))
    }

    fn validate(&self) -> Result<(), String> {
        match self {
            Self::NetworkOrigin(origin) if Self::network_origin(origin)? != *self => {
                Err("Network sharing rules must use a canonical origin without a path.".into())
            }
            Self::ModelProvider(provider) if provider.is_empty() => Err("A model provider identity is required.".into()),
            Self::MatrixRoom { account, room } if account.is_empty() || room.is_empty() => {
                Err("An account and room identity are required.".into())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum ContextId {
    App { account: String, app: String, room: Option<String> },
    PublicApp { account: String, app: String },
    Agent { account: String, room: String },
}

impl ContextId {
    pub fn account(&self) -> &str {
        match self { Self::App { account, .. } | Self::PublicApp { account, .. } | Self::Agent { account, .. } => account }
    }

    pub fn app(&self) -> Option<&str> {
        match self { Self::App { app, .. } | Self::PublicApp { app, .. } => Some(app), Self::Agent { .. } => None }
    }

    pub fn room(&self) -> Option<&str> {
        match self { Self::App { room, .. } => room.as_deref(), Self::Agent { room, .. } => Some(room), Self::PublicApp { .. } => None }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextSnapshot {
    pub context: ContextId,
    /// Zero denotes retained provenance without a live activation.
    pub epoch: u64,
    pub label: Label,
    pub clearance: Option<Label>,
    pub influences: Influences,
}

/// Additional recipients that may receive information derived from a source.
/// Empty means protected: room data can only return to its original room.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct FlowPolicy {
    pub recipients: BTreeSet<Recipient>,
}

impl FlowPolicy {
    pub fn allows(&self, recipient: &Recipient) -> bool {
        self.recipients.contains(recipient)
    }
}

/// One host registry. Durable provenance is independent of live contexts.
pub struct Registry {
    root: PathBuf,
    metadata: Metadata,
    contexts: BTreeSet<ContextId>,
    context_epochs: BTreeMap<ContextId, u64>,
    session_grants: Vec<SharingGrant>,
    session_effect_authorities: Vec<EffectAuthority>,
    effect_room_scope_matcher: Option<EffectRoomScopeMatcher>,
    authorities: Vec<ActionAuthority>,
    pending_actions: VecDeque<integrity::PendingAction>,
    pending_effects: VecDeque<effects::PendingEffect>,
    next_ephemeral_id: u64,
    decisions: RefCell<VecDeque<FlowDecision>>,
    action_decisions: RefCell<VecDeque<ActionDecision>>,
    persistence_error: Option<String>,
}

impl Registry {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, String> {
        let root = root.as_ref().to_path_buf();
        let metadata = match fs::read(root.join(METADATA_FILE)) {
            Ok(bytes) => storage::decode(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Metadata::default(),
            Err(error) => return Err(format!("Cannot read information-flow metadata: {error}")),
        };
        storage::validate_metadata(&metadata)?;
        Ok(Self {
            root, metadata, contexts: BTreeSet::new(), context_epochs: BTreeMap::new(), session_grants: Vec::new(), session_effect_authorities: Vec::new(), effect_room_scope_matcher: None, authorities: Vec::new(), pending_actions: VecDeque::new(), pending_effects: VecDeque::new(),
            next_ephemeral_id: 1 << 63, decisions: RefCell::new(VecDeque::new()),
            action_decisions: RefCell::new(VecDeque::new()), persistence_error: None,
        })
    }

    pub fn register_context(&mut self, context: &ContextId) -> Result<(), String> {
        self.register_context_with_legacy_data(context, false)
    }

    /// Register and durably record provenance before opening code or storage.
    pub fn register_context_with_legacy_data(&mut self, context: &ContextId, legacy_private_data: bool) -> Result<(), String> {
        self.check_healthy()?;
        validate_context(context)?;
        let needs_source_registration = context.app().is_some_and(|app| self.metadata.code.get(app)
            .is_none_or(|provenance| legacy_private_data && provenance == &StoredProvenance::default()));
        if self.contexts.contains(context) && !needs_source_registration { return self.labels(context).map(|_| ()); }
        let mut next = self.metadata.clone();
        if let Some(app) = context.app() {
            if !next.code.contains_key(app) {
                // Retired app_data files are kept for user-managed recovery,
                // but are never delivered to a new compartment. Their mere
                // presence does not make independently known code private.
                next.code.insert(app.to_owned(), StoredProvenance::legacy(legacy_private_data));
            } else if legacy_private_data && next.code.get(app) == Some(&StoredProvenance::default()) {
                // A verified stock working copy may have been replaced by
                // unrecorded source since its last launch. Preserve archived
                // sources and protect any additional unknown input, even if
                // this compartment is already registered.
                let mut provenance = next.historical_code.get(app).cloned().unwrap_or_default();
                let unknown = StoredProvenance::legacy(true);
                provenance.label.extend(unknown.label);
                provenance.influences.extend(unknown.influences);
                next.code.insert(app.to_owned(), provenance);
            }
        }
        if self.contexts.contains(context) {
            if next != self.metadata { self.persist(next)?; }
            return self.labels(context).map(|_| ());
        }
        if !next.contexts.iter().any(|entry| &entry.context == context) {
            // Unrecorded data in a compartment is never treated as public,
            // even if someone removed just its metadata entry.
            let unknown = (context.app().is_none() && legacy_private_data)
                || has_existing_data(&storage::context_path(&self.root, context))?;
            next.contexts.push(StoredContext {
                context: context.clone(), provenance: StoredProvenance::legacy(unknown),
                clearance: matches!(context, ContextId::PublicApp { .. }).then(Label::new),
            });
        }
        let entry = next.contexts.iter().find(|entry| &entry.context == context).unwrap();
        let label = storage::effective_provenance(&next, entry).label;
        check_clearance(context, entry.clearance.as_ref(), &label)?;
        self.persist(next)?;
        static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);
        let epoch = NEXT_EPOCH.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |next| next.checked_add(1))
            .map_err(|_| "Information-flow context activations exhausted.")?;
        self.context_epochs.insert(context.clone(), epoch);
        self.contexts.insert(context.clone());
        Ok(())
    }

    pub fn context_epoch(&self, context: &ContextId) -> Result<u64, String> {
        self.check_context(context)?;
        self.context_epochs.get(context).copied().ok_or_else(|| "Missing context activation identity.".into())
    }

    /// Reject asynchronous work captured by an earlier live instance even
    /// when its durable compartment has since reopened with identical labels.
    pub fn ensure_context_epoch(&self, context: &ContextId, epoch: u64) -> Result<(), String> {
        if self.context_epoch(context)? == epoch { Ok(()) }
        else { Err("The requesting context stopped or restarted.".into()) }
    }

    pub fn add_influences_for_activation(&mut self, context: &ContextId, epoch: u64, influences: impl IntoIterator<Item = Influence>) -> Result<(), String> {
        self.ensure_context_epoch(context, epoch)?;
        self.add_influences(context, influences)
    }

    pub fn add_sources_for_activation(&mut self, context: &ContextId, epoch: u64, sources: impl IntoIterator<Item = Source>) -> Result<(), String> {
        self.ensure_context_epoch(context, epoch)?;
        self.add_sources(context, sources)
    }

    pub fn ensure_allowed_for_activation(&self, context: &ContextId, epoch: u64, recipient: &Recipient) -> Result<(), String> {
        self.ensure_context_epoch(context, epoch)?;
        self.ensure_allowed(context, recipient)
    }

    pub fn ensure_action_allowed_for_activation(&self, context: &ContextId, epoch: u64, action: &SensitiveAction) -> Result<(), String> {
        self.ensure_context_epoch(context, epoch)?;
        self.ensure_action_allowed(context, action)
    }

    fn stored_context(&self, context: &ContextId) -> Result<&StoredContext, String> {
        self.check_context(context)?;
        self.metadata.contexts.iter().find(|entry| &entry.context == context)
            .ok_or_else(|| "Missing compartment information-flow provenance.".into())
    }

    pub fn labels(&self, context: &ContextId) -> Result<Label, String> {
        let entry = self.stored_context(context)?;
        let label = storage::effective_provenance(&self.metadata, entry).label;
        check_clearance(context, entry.clearance.as_ref(), &label)?;
        Ok(label)
    }

    pub fn influences(&self, context: &ContextId) -> Result<Influences, String> {
        let entry = self.stored_context(context)?;
        self.labels(context)?;
        Ok(storage::effective_provenance(&self.metadata, entry).influences)
    }

    pub fn contexts(&self) -> Result<Vec<ContextSnapshot>, String> {
        self.check_healthy()?;
        // Trusted UI may inspect even a context blocked by a changed code floor.
        Ok(self.metadata.contexts.iter().filter(|entry| self.contexts.contains(&entry.context)).map(|entry| {
            let provenance = storage::effective_provenance(&self.metadata, entry);
            ContextSnapshot { context: entry.context.clone(), epoch: self.context_epochs[&entry.context], label: provenance.label,
                clearance: entry.clearance.clone(), influences: provenance.influences }
        }).collect())
    }

    /// Inspect durable provenance, including compartments that have stopped.
    /// Inactive snapshots use epoch zero and never authorize an operation.
    pub fn retained_contexts(&self) -> Result<Vec<ContextSnapshot>, String> {
        self.check_healthy()?;
        Ok(self.metadata.contexts.iter().map(|entry| {
            let provenance = storage::effective_provenance(&self.metadata, entry);
            let epoch = if self.contexts.contains(&entry.context) {
                self.context_epochs.get(&entry.context).copied().unwrap_or(0)
            } else { 0 };
            ContextSnapshot { context: entry.context.clone(), epoch, label: provenance.label,
                clearance: entry.clearance.clone(), influences: provenance.influences }
        }).collect())
    }

    pub fn context_storage_path(&self, context: &ContextId) -> Result<PathBuf, String> {
        self.labels(context)?;
        Ok(storage::context_path(&self.root, context))
    }

    /// Retained legacy files and every recorded private/public compartment.
    /// Clearing these files does not erase their durable provenance.
    pub fn app_storage_paths(&self, app: &str) -> Result<Vec<PathBuf>, String> {
        self.check_healthy()?;
        validate_app(app)?;
        let mut paths = vec![self.root.join("app_data").join(app)];
        paths.extend(self.metadata.contexts.iter().filter(|entry| entry.context.app() == Some(app))
            .map(|entry| storage::context_path(&self.root, &entry.context)));
        Ok(paths)
    }

    pub fn clearance(&self, context: &ContextId) -> Result<Option<Label>, String> {
        Ok(self.stored_context(context)?.clearance.clone())
    }

    /// A clearance limits future input; it never removes sources already read.
    pub fn set_clearance(&mut self, context: &ContextId, clearance: Option<Label>) -> Result<(), String> {
        let label = storage::effective_provenance(&self.metadata, self.stored_context(context)?).label;
        if let Some(bound) = &clearance { for source in bound { validate_source(source)?; } }
        if matches!(context, ContextId::PublicApp { .. }) && clearance != Some(Label::new()) {
            return Err("A public app's clearance must remain empty.".into());
        }
        check_clearance(context, clearance.as_ref(), &label)?;
        let mut next = self.metadata.clone();
        next.contexts.iter_mut().find(|entry| &entry.context == context).unwrap().clearance = clearance;
        self.persist(next)
    }

    /// Join both dimensions atomically before any corresponding input delivery.
    fn join_provenance(&mut self, context: &ContextId, incoming: StoredProvenance) -> Result<(), String> {
        let mut label = self.labels(context)?;
        for source in &incoming.label { validate_source(source)?; }
        for influence in &incoming.influences { integrity::validate_influence(influence)?; }
        label.extend(incoming.label.iter().cloned());
        check_clearance(context, self.stored_context(context)?.clearance.as_ref(), &label)?;
        let mut next = self.metadata.clone();
        let entry = next.contexts.iter_mut().find(|entry| &entry.context == context).unwrap();
        let before = entry.provenance.clone();
        entry.provenance.label.extend(incoming.label);
        entry.provenance.influences.extend(incoming.influences);
        if entry.provenance == before { return Ok(()); }
        self.persist(next)
    }

    pub fn add_sources(&mut self, context: &ContextId, sources: impl IntoIterator<Item = Source>) -> Result<(), String> {
        self.join_provenance(context, provenance_for_sources(sources))
    }

    pub fn add_influences(&mut self, context: &ContextId, influences: impl IntoIterator<Item = Influence>) -> Result<(), String> {
        self.join_provenance(context, StoredProvenance { label: Label::new(), influences: influences.into_iter().collect() })
    }

    /// Transfers confidentiality and integrity before IPC/tool data delivery.
    pub fn transfer(&mut self, sender: &ContextId, receiver: &ContextId) -> Result<(), String> {
        let label = self.labels(sender)?;
        let mut influences = self.influences(sender)?;
        if sender != receiver && let Some(app) = sender.app() {
            influences.insert(Influence::MiniApp { account: sender.account().into(), app: app.into() });
        }
        self.join_provenance(receiver, StoredProvenance { label, influences })
    }

    pub fn code_labels(&self, app: &str) -> Result<Label, String> {
        self.check_healthy()?;
        validate_app(app)?;
        let mut label = self.metadata.code.get(app).map(|entry| entry.label.clone())
            .ok_or("Missing app code provenance; register its origin first.")?;
        if let Some(history) = self.metadata.historical_code.get(app) { label.extend(history.label.iter().cloned()); }
        Ok(label)
    }

    pub fn code_influences(&self, app: &str) -> Result<Influences, String> {
        self.code_labels(app)?;
        let mut influences = self.metadata.code.get(app).unwrap().influences.clone();
        if let Some(history) = self.metadata.historical_code.get(app) { influences.extend(history.influences.iter().cloned()); }
        Ok(influences)
    }

    /// Trusted host replacement by this build's exact built-in source.
    ///
    /// Archive the old code floor for history reads/restores. Running contexts
    /// and compartments with saved files retain their complete old provenance;
    /// only stopped, empty compartments start fresh with the known stock code.
    /// The caller must verify the complete working manifest against the stock
    /// built into the executable before invoking this recovery operation.
    pub fn reconcile_builtin_code(&mut self, app: &str) -> Result<(), String> {
        self.check_healthy()?;
        validate_app(app)?;
        let old_code = self.metadata.code.get(app).cloned().unwrap_or_default();
        let mut next = self.metadata.clone();
        if old_code != StoredProvenance::default() {
            let history = next.historical_code.entry(app.into()).or_default();
            history.label.extend(old_code.label.iter().cloned());
            history.influences.extend(old_code.influences.iter().cloned());
        }
        for entry in next.contexts.iter_mut().filter(|entry| entry.context.app() == Some(app)) {
            if !self.contexts.contains(&entry.context) && !has_existing_data(&storage::context_path(&self.root, &entry.context))? {
                entry.provenance = StoredProvenance::default();
            } else {
                entry.provenance.label.extend(old_code.label.iter().cloned());
                entry.provenance.influences.extend(old_code.influences.iter().cloned());
            }
        }
        next.code.insert(app.into(), StoredProvenance::default());
        if next == self.metadata { return Ok(()); }
        self.persist(next)
    }

    /// Restore the archived floor before executing any historical source.
    ///
    /// History is not assumed public because the current working copy has
    /// been replaced with stock code. Call before installing a saved version.
    pub fn restore_app_code_provenance(&mut self, app: &str) -> Result<(), String> {
        self.code_labels(app)?;
        let history = self.metadata.historical_code.get(app).cloned().unwrap_or_default();
        self.join_code(app, history)
    }

    fn join_code(&mut self, app: &str, incoming: StoredProvenance) -> Result<(), String> {
        self.check_healthy()?;
        validate_app(app)?;
        for source in &incoming.label { validate_source(source)?; }
        for influence in &incoming.influences { integrity::validate_influence(influence)?; }
        let mut next = self.metadata.clone();
        let entry = next.code.entry(app.into()).or_default();
        entry.label.extend(incoming.label);
        entry.influences.extend(incoming.influences);
        // A code change may exceed an already-open public context's clearance.
        // Its next boundary check then fails; the code floor is never lowered.
        self.persist(next)
    }

    pub fn add_code_sources(&mut self, app: &str, sources: impl IntoIterator<Item = Source>) -> Result<(), String> {
        self.join_code(app, provenance_for_sources(sources))
    }

    pub fn add_code_influences(&mut self, app: &str, influences: impl IntoIterator<Item = Influence>) -> Result<(), String> {
        self.join_code(app, StoredProvenance { label: Label::new(), influences: influences.into_iter().collect() })
    }

    pub fn record_app_code_from(&mut self, app: &str, context: &ContextId) -> Result<(), String> {
        let label = self.labels(context)?;
        let mut influences = self.influences(context)?;
        if let Some(source_app) = context.app() {
            influences.insert(Influence::MiniApp { account: context.account().into(), app: source_app.into() });
        }
        self.join_code(app, StoredProvenance { label, influences })
    }

    /// Joins an app's shared code provenance into `receiver`, fail-closed: an
    /// app whose code has `UnknownPrivate`/`Unknown` provenance taints the
    /// receiver rather than having those sources silently dropped. Metadata
    /// listings do not call this; only paths that expose app code do.
    pub fn join_code_labels(&mut self, app: &str, receiver: &ContextId) -> Result<(), String> {
        let labels = self.code_labels(app)?;
        self.add_sources(receiver, labels)?;
        let influences = self.code_influences(app)?;
        self.add_influences(receiver, influences)
    }

    /// Drops a stale session's accumulated provenance, then restores the
    /// baseline own-room source and influence. Only a fresh session start
    /// calls this; ordinary tool calls must never reset the label.
    pub fn begin_agent_session(&mut self, context: &ContextId) -> Result<(), String> {
        self.reset_agent_session_provenance(context)?;
        let room = context.room().ok_or("An agent session context needs a room.")?;
        self.add_sources(context, [Source::Room { account: context.account().into(), room: room.into() }])?;
        self.add_influences(context, [Influence::RoomContent { account: context.account().into(), room: room.into() }])
    }

    pub fn remove_context(&mut self, context: &ContextId) {
        self.forget_exact_actions(context);
        self.pending_effects.retain(|pending| &pending.review.context != context);
        self.contexts.remove(context);
        self.context_epochs.remove(context);
        // A stopped worker loses its exact approvals. Explicit session choices
        // keep their advertised duration when that same context starts again.
        self.authorities.retain(|grant| &grant.context != context
            || !matches!(grant.session, AuthoritySession::Once { .. }));
    }

    /// A late destructor must never retire a newer activation of this identity.
    pub fn remove_context_for_activation(&mut self, context: &ContextId, expected_epoch: u64) -> Result<(), String> {
        self.ensure_context_epoch(context, expected_epoch)?;
        self.remove_context(context);
        Ok(())
    }

    /// Reset a fresh agent session's accumulated provenance to the empty label.
    ///
    /// Agent memory is per-session and does not survive a restart (the
    /// embedded backend uses an in-memory store and the confined child uses a
    /// disposable per-session workspace), so a previous session's label is not
    /// a record of live data. Carrying it forward only lets one bad join — a
    /// legacy app's `Source::UnknownPrivate` code provenance picked up by
    /// `list_apps` — permanently wedge every later model call and room write.
    /// The caller re-adds the baseline room source immediately after.
    pub fn reset_agent_session_provenance(&mut self, context: &ContextId) -> Result<(), String> {
        self.check_healthy()?;
        if !matches!(context, ContextId::Agent { .. }) {
            return Err("Only an agent session context may have its provenance reset.".into());
        }
        let mut next = self.metadata.clone();
        let Some(entry) = next.contexts.iter_mut().find(|entry| &entry.context == context) else {
            return Ok(());
        };
        entry.provenance = StoredProvenance::default();
        self.persist(next)
    }

    pub fn close_room_session(&mut self, account: &str, room: &str) -> Result<(), String> {
        self.check_healthy()?;
        self.close_exact_room(account, room);
        self.pending_effects.retain(|pending| pending.review.context.account() != account || pending.review.context.room() != Some(room));
        self.session_grants.retain(|grant| !matches!(&grant.duration,
            SharingDuration::RoomSession { account: a, room: r } if a == account && r == room));
        self.session_effect_authorities.retain(|grant| !matches!(&grant.duration,
            SharingDuration::RoomSession { account: a, room: r } if a == account && r == room));
        self.authorities.retain(|grant| !matches!(&grant.session,
            AuthoritySession::RoomSession { account: a, room: r } if a == account && r == room));
        Ok(())
    }

    pub fn end_session(&mut self) -> Result<(), String> {
        self.check_healthy()?;
        self.session_grants.clear();
        self.session_effect_authorities.clear();
        self.authorities.clear();
        self.pending_actions.clear();
        self.pending_effects.clear();
        self.action_decisions.borrow_mut().clear();
        self.contexts.clear();
        self.context_epochs.clear();
        Ok(())
    }

    /// Revoke all durable and session approvals without erasing the sources
    /// or influences protecting retained app data, code, and agent history.
    /// Retire activations too, so an already queued worker cannot reuse consent.
    pub fn reset_permissions(&mut self) -> Result<(), String> {
        self.check_healthy()?;
        let mut next = self.metadata.clone();
        next.grants.clear();
        next.effect_authorities.clear();
        self.end_session()?;
        if next != self.metadata { self.persist(next)?; }
        Ok(())
    }

    fn check_healthy(&self) -> Result<(), String> {
        match &self.persistence_error { Some(error) => Err(error.clone()), None => Ok(()) }
    }

    fn check_context(&self, context: &ContextId) -> Result<(), String> {
        self.check_healthy()?;
        if self.contexts.contains(context) { Ok(()) }
        else { Err("Information-flow context is not registered; access is blocked.".into()) }
    }

    fn ephemeral_id(&mut self) -> Result<u64, String> {
        let id = self.next_ephemeral_id;
        self.next_ephemeral_id = id.checked_add(1).ok_or("Information-flow grant identities exhausted.")?;
        Ok(id)
    }

    fn persist(&mut self, next: Metadata) -> Result<(), String> {
        let result = serde_json::to_vec_pretty(&next).map_err(std::io::Error::other)
            .and_then(|bytes| save_metadata(&self.root, &bytes));
        match result {
            Ok(()) => { self.metadata = next; Ok(()) }
            Err(error) => {
                let error = format!("Cannot persist information-flow protection; private data access and output are blocked: {error}");
                self.persistence_error = Some(error.clone());
                Err(error)
            }
        }
    }
}

fn validate_source(source: &Source) -> Result<(), String> {
    match source {
        Source::Account { account } if account.is_empty() => Err("A source account identity is required.".into()),
        Source::Room { account, room } if account.is_empty() || room.is_empty() => Err("A source account and room identity are required.".into()),
        Source::RoomDirectory { account } if account.is_empty() => Err("A source account identity is required.".into()),
        _ => Ok(()),
    }
}

fn validate_app(app: &str) -> Result<(), String> {
    let mut components = Path::new(app).components();
    if matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none() { Ok(()) }
    else { Err("Invalid host information-flow app identity.".into()) }
}

fn validate_context(context: &ContextId) -> Result<(), String> {
    if context.account().is_empty() || context.room().is_some_and(str::is_empty) {
        return Err("Invalid host information-flow context.".into());
    }
    if let Some(app) = context.app() { validate_app(app)?; }
    Ok(())
}

fn check_clearance(context: &ContextId, clearance: Option<&Label>, label: &Label) -> Result<(), String> {
    if (matches!(context, ContextId::PublicApp { .. }) && !label.is_empty())
        || clearance.is_some_and(|clearance| !label.is_subset(clearance))
    {
        return Err("The requested input exceeds this context's information-flow clearance.".into());
    }
    Ok(())
}

fn provenance_for_sources(sources: impl IntoIterator<Item = Source>) -> StoredProvenance {
    let label: Label = sources.into_iter().collect();
    let influences = label.iter().filter_map(|source| match source {
        Source::Room { account, room } => Some(Influence::RoomContent { account: account.clone(), room: room.clone() }),
        Source::RoomDirectory { account } => Some(Influence::RoomDirectory { account: account.clone() }),
        Source::UnknownPrivate => Some(Influence::Unknown),
        Source::Account { .. } => None,
    }).collect();
    StoredProvenance { label, influences }
}

fn has_existing_data(path: &Path) -> Result<bool, String> {
    match fs::read_dir(path) {
        Ok(mut entries) => match entries.next() {
            Some(Ok(_)) => Ok(true),
            Some(Err(error)) => Err(format!("Cannot inspect existing private app storage: {error}")),
            None => Ok(false),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("Cannot inspect existing private app storage: {error}")),
    }
}

fn save_metadata(root: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static NEXT_SAVE: AtomicU64 = AtomicU64::new(0);
    fs::create_dir_all(root)?;
    let temporary = root.join(format!(".{METADATA_FILE}.{}.{}.tmp", std::process::id(), NEXT_SAVE.fetch_add(1, Ordering::Relaxed)));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, root.join(METADATA_FILE))?;
        // Unix directory fsync makes the rename durable, not just its contents.
        #[cfg(unix)]
        fs::File::open(root)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() { let _ = fs::remove_file(&temporary); }
    result
}

static GLOBAL: OnceLock<Mutex<Result<Registry, String>>> = OnceLock::new();

/// Initialize once after choosing the host's data root. A corrupt file blocks
/// the registry; it must never silently reset private contexts to public.
pub fn init(root: impl AsRef<Path>) -> Result<(), String> {
    let root = root.as_ref();
    let registry = GLOBAL.get_or_init(|| Mutex::new(Registry::open(root)));
    let guard = registry.lock().map_err(|_| "Information-flow registry lock failed.".to_owned())?;
    match guard.as_ref() {
        Ok(registry) if registry.root == root => registry.check_healthy(),
        Ok(_) => Err("Information-flow registry was already initialized for another data root.".into()),
        Err(error) => Err(error.clone()),
    }
}

fn with_registry<T>(operation: impl FnOnce(&mut Registry) -> Result<T, String>) -> Result<T, String> {
    let registry = GLOBAL.get_or_init(|| Mutex::new(Registry::open(crate::data_root())));
    let mut guard = registry.lock().map_err(|_| "Information-flow registry lock failed.".to_owned())?;
    match guard.as_mut() { Ok(registry) => operation(registry), Err(error) => Err(error.clone()) }
}

pub fn register_context(context: &ContextId) -> Result<(), String> {
    with_registry(|registry| registry.register_context(context))
}

pub fn register_context_with_legacy_data(context: &ContextId, legacy_private_data: bool) -> Result<(), String> {
    with_registry(|registry| registry.register_context_with_legacy_data(context, legacy_private_data))
}

pub fn add_sources(context: &ContextId, sources: impl IntoIterator<Item = Source>) -> Result<(), String> {
    with_registry(|registry| registry.add_sources(context, sources))
}

/// Drop a fresh agent session's accumulated provenance; see
/// [`Registry::reset_agent_session_provenance`].
pub fn reset_agent_session_provenance(context: &ContextId) -> Result<(), String> {
    with_registry(|registry| registry.reset_agent_session_provenance(context))
}

/// Drop a fresh agent session's accumulated provenance and restore its own
/// room baseline; see [`Registry::begin_agent_session`].
pub fn begin_agent_session(context: &ContextId) -> Result<(), String> {
    with_registry(|registry| registry.begin_agent_session(context))
}

pub fn join_labels(context: &ContextId, label: &Label) -> Result<(), String> {
    add_sources(context, label.iter().cloned())
}

pub fn transfer(sender: &ContextId, receiver: &ContextId) -> Result<(), String> {
    with_registry(|registry| registry.transfer(sender, receiver))
}

pub fn labels(context: &ContextId) -> Result<Label, String> {
    with_registry(|registry| registry.labels(context))
}

pub fn ensure_allowed(context: &ContextId, recipient: &Recipient) -> Result<(), String> {
    with_registry(|registry| registry.ensure_allowed(context, recipient))
}

pub fn ensure_labels_allowed(label: &Label, recipient: &Recipient) -> Result<(), String> {
    with_registry(|registry| registry.ensure_labels_allowed(label, recipient))
}

pub fn set_policy(source: Source, policy: FlowPolicy) -> Result<(), String> {
    with_registry(|registry| registry.set_policy(source, policy))
}

pub fn policies() -> Result<BTreeMap<Source, FlowPolicy>, String> {
    with_registry(|registry| registry.policies())
}

pub fn policy_for(source: &Source) -> Result<FlowPolicy, String> {
    with_registry(|registry| registry.policy_for(source))
}

pub fn remove_context(context: &ContextId) -> Result<(), String> {
    with_registry(|registry| { registry.remove_context(context); Ok(()) })
}

pub fn remove_context_for_activation(context: &ContextId, expected_epoch: u64) -> Result<(), String> {
    with_registry(|registry| registry.remove_context_for_activation(context, expected_epoch))
}

pub fn context_storage_path(context: &ContextId) -> Result<PathBuf, String> {
    with_registry(|registry| registry.context_storage_path(context))
}

pub fn context_epoch(context: &ContextId) -> Result<u64, String> {
    with_registry(|registry| registry.context_epoch(context))
}

pub fn prepare_effect_for_activation(context: &ContextId, epoch: u64, recipient: Option<&Recipient>, action: Option<&SensitiveAction>, payload: &serde_json::Value) -> Result<EffectReview, String> {
    with_registry(|registry| registry.prepare_effect_for_activation(context, epoch, recipient, action, payload))
}

pub fn check_effect_for_activation(context: &ContextId, epoch: u64, recipient: Option<&Recipient>, action: Option<&SensitiveAction>, payload: &serde_json::Value) -> Result<(), String> {
    with_registry(|registry| registry.check_effect_for_activation(context, epoch, recipient, action, payload))
}

pub fn approve_effect_once(review: &EffectReview) -> Result<(), String> {
    with_registry(|registry| registry.approve_effect_once(review))
}

pub fn approve_effect_session(review: &EffectReview, duration: SharingDuration) -> Result<(), String> {
    with_registry(|registry| registry.approve_effect_session(review, duration))
}

pub fn approve_effect_always(review: &EffectReview) -> Result<(), String> {
    with_registry(|registry| registry.approve_effect_always(review))
}

pub fn set_effect_room_scope_matcher(matcher: EffectRoomScopeMatcher) -> Result<(), String> {
    with_registry(|registry| registry.set_effect_room_scope_matcher(matcher))
}

pub fn approve_effect_scoped(review: &EffectReview, scope: crate::permissions::RoomScope, duration: SharingDuration) -> Result<(), String> {
    with_registry(|registry| registry.approve_effect_scoped(review, scope, duration))
}

pub fn effect_authorities() -> Result<Vec<EffectAuthority>, String> {
    with_registry(|registry| registry.effect_authorities())
}

pub fn revoke_effect_authority(id: u64) -> Result<bool, String> {
    with_registry(|registry| registry.revoke_effect_authority(id))
}

pub fn commit_effect_for_activation(context: &ContextId, epoch: u64, recipient: Option<&Recipient>, action: Option<&SensitiveAction>, payload: &serde_json::Value) -> Result<(), String> {
    with_registry(|registry| registry.commit_effect_for_activation(context, epoch, recipient, action, payload))
}

pub fn cancel_effect(id: u64) -> Result<bool, String> {
    with_registry(|registry| registry.cancel_effect(id))
}

pub fn ensure_context_epoch(context: &ContextId, epoch: u64) -> Result<(), String> {
    with_registry(|registry| registry.ensure_context_epoch(context, epoch))
}

pub fn add_influences_for_activation(context: &ContextId, epoch: u64, influences: impl IntoIterator<Item = Influence>) -> Result<(), String> {
    with_registry(|registry| registry.add_influences_for_activation(context, epoch, influences))
}

pub fn add_sources_for_activation(context: &ContextId, epoch: u64, sources: impl IntoIterator<Item = Source>) -> Result<(), String> {
    with_registry(|registry| registry.add_sources_for_activation(context, epoch, sources))
}

pub fn ensure_allowed_for_activation(context: &ContextId, epoch: u64, recipient: &Recipient) -> Result<(), String> {
    with_registry(|registry| registry.ensure_allowed_for_activation(context, epoch, recipient))
}

pub fn ensure_action_allowed_for_activation(context: &ContextId, epoch: u64, action: &SensitiveAction) -> Result<(), String> {
    with_registry(|registry| registry.ensure_action_allowed_for_activation(context, epoch, action))
}

pub fn app_storage_paths(app: &str) -> Result<Vec<PathBuf>, String> {
    with_registry(|registry| registry.app_storage_paths(app))
}

pub fn contexts() -> Result<Vec<ContextSnapshot>, String> {
    with_registry(|registry| registry.contexts())
}

pub fn retained_contexts() -> Result<Vec<ContextSnapshot>, String> {
    with_registry(|registry| registry.retained_contexts())
}

pub fn clearance(context: &ContextId) -> Result<Option<Label>, String> {
    with_registry(|registry| registry.clearance(context))
}

pub fn set_clearance(context: &ContextId, clearance: Option<Label>) -> Result<(), String> {
    with_registry(|registry| registry.set_clearance(context, clearance))
}

pub fn influences(context: &ContextId) -> Result<Influences, String> {
    with_registry(|registry| registry.influences(context))
}

pub fn add_influences(context: &ContextId, influences: impl IntoIterator<Item = Influence>) -> Result<(), String> {
    with_registry(|registry| registry.add_influences(context, influences))
}

pub fn code_labels(app: &str) -> Result<Label, String> {
    with_registry(|registry| registry.code_labels(app))
}

pub fn code_influences(app: &str) -> Result<Influences, String> {
    with_registry(|registry| registry.code_influences(app))
}

pub fn reconcile_builtin_code(app: &str) -> Result<(), String> {
    with_registry(|registry| registry.reconcile_builtin_code(app))
}

pub fn restore_app_code_provenance(app: &str) -> Result<(), String> {
    with_registry(|registry| registry.restore_app_code_provenance(app))
}

pub fn add_code_sources(app: &str, sources: impl IntoIterator<Item = Source>) -> Result<(), String> {
    with_registry(|registry| registry.add_code_sources(app, sources))
}

pub fn add_code_influences(app: &str, influences: impl IntoIterator<Item = Influence>) -> Result<(), String> {
    with_registry(|registry| registry.add_code_influences(app, influences))
}

pub fn record_app_code_from(app: &str, context: &ContextId) -> Result<(), String> {
    with_registry(|registry| registry.record_app_code_from(app, context))
}

pub fn join_code_labels(app: &str, receiver: &ContextId) -> Result<(), String> {
    with_registry(|registry| registry.join_code_labels(app, receiver))
}

pub fn grant_sharing(source: Source, recipient: Recipient, reader: ReaderScope, duration: SharingDuration) -> Result<u64, String> {
    with_registry(|registry| registry.grant_sharing(source, recipient, reader, duration))
}

pub fn revoke_sharing(id: u64) -> Result<bool, String> {
    with_registry(|registry| registry.revoke_sharing(id))
}

pub fn sharing_grants() -> Result<Vec<SharingGrant>, String> {
    with_registry(|registry| registry.sharing_grants())
}

/// Whether `source` may already reach `recipient` for this exact agent/reader
/// context, without joining any new source into the context's label. Used by
/// the upfront task-plan resolver to turn a needed information-flow rule into
/// `AlreadyAllowed` instead of `NeedsGrant`; enforcement still happens where it
/// always did (the guarded model transport and the room/network workers).
pub fn sharing_allows_for_reader(source: &Source, recipient: &Recipient, reader: &ContextId) -> Result<bool, String> {
    with_registry(|registry| {
        registry.check_healthy()?;
        recipient.validate()?;
        validate_source(source)?;
        Ok(registry.source_allowed(source, recipient, Some(reader)))
    })
}

pub fn close_room_session(account: &str, room: &str) -> Result<(), String> {
    with_registry(|registry| registry.close_room_session(account, room))
}

pub fn end_session() -> Result<(), String> {
    with_registry(Registry::end_session)
}

pub fn reset_permissions() -> Result<(), String> {
    with_registry(Registry::reset_permissions)
}

pub fn decision(context: &ContextId, recipient: &Recipient) -> Result<FlowDecision, String> {
    with_registry(|registry| registry.decision(context, recipient))
}

pub fn recent_decisions() -> Result<Vec<FlowDecision>, String> {
    with_registry(|registry| registry.recent_decisions())
}

pub fn action_decision(context: &ContextId, action: &SensitiveAction) -> Result<ActionDecision, String> {
    with_registry(|registry| registry.action_decision(context, action))
}

pub fn ensure_action_allowed(context: &ContextId, action: &SensitiveAction) -> Result<(), String> {
    with_registry(|registry| registry.ensure_action_allowed(context, action))
}

pub fn recent_action_decisions() -> Result<Vec<ActionDecision>, String> {
    with_registry(|registry| registry.recent_action_decisions())
}

pub fn grant_authority(context: &ContextId, action: SensitiveAction, session: AuthoritySession) -> Result<u64, String> {
    with_registry(|registry| registry.grant_authority(context, action, session))
}

pub fn grant_authority_checked(context: &ContextId, action: SensitiveAction, session: AuthoritySession, expected: &Influences) -> Result<u64, String> {
    with_registry(|registry| registry.grant_authority_checked(context, action, session, expected))
}

pub fn grant_authority_for_activation(context: &ContextId, action: SensitiveAction, session: AuthoritySession, expected: &Influences, expected_epoch: u64) -> Result<u64, String> {
    with_registry(|registry| registry.grant_authority_for_activation(context, action, session, expected, expected_epoch))
}

pub fn check_exact_action_for_activation(context: &ContextId, epoch: u64, action: &SensitiveAction, payload: &serde_json::Value) -> Result<(), String> {
    with_registry(|registry| registry.check_exact_action_for_activation(context, epoch, action, payload))
}

pub fn commit_exact_action_for_activation(context: &ContextId, epoch: u64, action: &SensitiveAction, payload: &serde_json::Value) -> Result<(), String> {
    with_registry(|registry| registry.commit_exact_action_for_activation(context, epoch, action, payload))
}

pub fn grant_exact_action_for_activation(context: &ContextId, request_id: u64, expected: &Influences, epoch: u64) -> Result<u64, String> {
    with_registry(|registry| registry.grant_exact_action_for_activation(context, request_id, expected, epoch))
}

pub fn cancel_exact_action(request_id: u64) -> Result<bool, String> {
    with_registry(|registry| registry.cancel_exact_action(request_id))
}

pub fn authorities() -> Result<Vec<ActionAuthority>, String> {
    with_registry(|registry| registry.authorities())
}

pub fn revoke_authority(id: u64) -> Result<bool, String> {
    with_registry(|registry| registry.revoke_authority(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) struct TestRoot(pub(super) PathBuf);

    impl TestRoot {
        pub(super) fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!("robrix-ifc-test-{}-{}-{}",
                std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        pub(super) fn registry(&self) -> Registry { Registry::open(&self.0).unwrap() }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }

    fn app(id: &str, account: &str, room: &str) -> ContextId {
        ContextId::App { account: account.into(), app: id.into(), room: Some(room.into()) }
    }

    fn agent(account: &str, room: &str) -> ContextId {
        ContextId::Agent { account: account.into(), room: room.into() }
    }

    fn room_source(account: &str, room: &str) -> Source {
        Source::Room { account: account.into(), room: room.into() }
    }

    fn room_recipient(account: &str, room: &str) -> Recipient {
        Recipient::MatrixRoom { account: account.into(), room: room.into() }
    }

    fn site() -> Recipient { Recipient::network_origin("https://example.com/resource").unwrap() }

    fn allow(recipient: Recipient) -> FlowPolicy {
        FlowPolicy { recipients: [recipient].into_iter().collect() }
    }

    #[test]
    fn public_context_has_no_flow_restriction_but_unregistered_is_blocked() {
        let root = TestRoot::new();
        let mut registry = root.registry();
        let context = app("test", "alice", "room-a");
        assert!(registry.ensure_allowed(&context, &site()).is_err());
        registry.register_context(&context).unwrap();
        assert!(registry.ensure_allowed(&context, &site()).is_ok());
        assert!(registry.ensure_allowed(&context, &Recipient::External).is_ok());
    }

    #[test]
    fn private_context_blocks_every_output_without_inspecting_encoded_content() {
        let root = TestRoot::new();
        let mut registry = root.registry();
        let context = app("test", "alice", "room-a");
        registry.register_context(&context).unwrap();
        registry.add_sources(&context, [room_source("alice", "room-a")]).unwrap();
        // No payload is passed to this decision: encoding, paraphrasing, query
        // parameters, or splitting a secret cannot change the context's label.
        for recipient in [site(), Recipient::ModelProvider("provider/account-1".into()), Recipient::External] {
            assert!(registry.ensure_allowed(&context, &recipient).is_err());
        }
        assert!(registry.ensure_allowed(&context, &room_recipient("alice", "room-a")).is_ok());
        assert!(registry.ensure_allowed(&context, &room_recipient("alice", "room-b")).is_err());
    }

    #[test]
    fn every_source_must_allow_the_actual_recipient() {
        let root = TestRoot::new();
        let mut registry = root.registry();
        let context = agent("alice", "room-a");
        let first = room_source("alice", "room-a");
        let second = room_source("alice", "room-b");
        registry.register_context(&context).unwrap();
        registry.add_sources(&context, [first.clone(), second.clone()]).unwrap();
        registry.set_policy(first, allow(site())).unwrap();
        assert!(registry.ensure_allowed(&context, &site()).is_err());
        registry.set_policy(second.clone(), allow(site())).unwrap();
        assert!(registry.ensure_allowed(&context, &site()).is_ok());
        registry.set_policy(second, FlowPolicy::default()).unwrap();
        assert!(registry.ensure_allowed(&context, &site()).is_err(), "revocation must take effect in existing contexts");
    }

    #[test]
    fn account_data_requires_separate_consent_from_room_data() {
        let root = TestRoot::new();
        let mut registry = root.registry();
        let context = app("test", "alice", "room-a");
        let room = room_source("alice", "room-a");
        let account = Source::Account { account: "alice".into() };
        registry.register_context(&context).unwrap();
        registry.add_sources(&context, [room.clone(), account.clone()]).unwrap();
        registry.set_policy(room, allow(site())).unwrap();
        assert!(registry.ensure_allowed(&context, &site()).is_err());
        assert!(registry.ensure_allowed(&context, &room_recipient("alice", "room-a")).is_err());
        registry.set_policy(account, allow(site())).unwrap();
        assert!(registry.ensure_allowed(&context, &site()).is_ok());
    }

    #[test]
    fn labels_grow_monotonically_and_do_not_reset_when_reregistered() {
        let root = TestRoot::new();
        let mut registry = root.registry();
        let context = agent("alice", "room-a");
        registry.register_context(&context).unwrap();
        registry.add_sources(&context, [room_source("alice", "room-a")]).unwrap();
        registry.add_sources(&context, []).unwrap();
        registry.register_context(&context).unwrap();
        registry.add_sources(&context, [room_source("alice", "room-b")]).unwrap();
        assert_eq!(registry.labels(&context).unwrap().len(), 2);
        registry.remove_context(&context);
        assert!(registry.labels(&context).is_err());
        registry.register_context(&context).unwrap();
        assert_eq!(registry.labels(&context).unwrap().len(), 2);
    }

    #[test]
    fn source_room_identity_includes_account() {
        let root = TestRoot::new();
        let mut registry = root.registry();
        let context = app("test", "alice", "room-a");
        registry.register_context(&context).unwrap();
        registry.add_sources(&context, [room_source("alice", "room-a")]).unwrap();
        assert!(registry.ensure_allowed(&context, &room_recipient("bob", "room-a")).is_err());
        registry.set_policy(room_source("bob", "room-a"), allow(site())).unwrap();
        assert!(registry.ensure_allowed(&context, &site()).is_err());
    }

    #[test]
    fn compartment_storage_separates_rooms_and_accounts_but_code_remains_shared() {
        let root = TestRoot::new();
        let mut registry = root.registry();
        let first = app("shared-app", "alice", "room-a");
        let second = app("shared-app", "bob", "room-b");
        let other_app = app("different-app", "bob", "room-b");
        for context in [&first, &second, &other_app] { registry.register_context(context).unwrap(); }
        registry.add_sources(&first, [room_source("alice", "room-a")]).unwrap();
        assert!(registry.labels(&second).unwrap().is_empty());
        assert!(registry.ensure_allowed(&second, &site()).is_ok());
        assert!(registry.ensure_allowed(&other_app, &site()).is_ok());
        registry.add_sources(&second, [room_source("bob", "room-b")]).unwrap();
        assert_eq!(registry.labels(&first).unwrap().len(), 1);
        assert_ne!(registry.context_storage_path(&first).unwrap(), registry.context_storage_path(&second).unwrap());
        registry.record_app_code_from("shared-app", &first).unwrap();
        assert_eq!(registry.labels(&second).unwrap().len(), 2);
        assert!(registry.labels(&other_app).unwrap().is_empty());
    }

    #[test]
    fn restart_retains_app_and_agent_provenance_and_rules() {
        let root = TestRoot::new();
        let context = app("test", "alice", "room-a");
        let agent = agent("alice", "room-a");
        let source = room_source("alice", "room-b");
        {
            let mut registry = root.registry();
            for context in [&context, &agent] {
                registry.register_context(context).unwrap();
                registry.add_sources(context, [source.clone()]).unwrap();
            }
            registry.set_policy(source.clone(), allow(site())).unwrap();
        }
        let mut restored = root.registry();
        for context in [&context, &agent] {
            restored.register_context(context).unwrap();
            assert_eq!(restored.labels(context).unwrap(), [source.clone()].into_iter().collect());
            assert!(restored.ensure_allowed(context, &site()).is_ok());
            assert!(restored.ensure_allowed(context, &Recipient::External).is_err());
        }
    }

    #[test]
    fn retained_context_inspection_survives_stop_and_restart_without_authority() {
        let root = TestRoot::new();
        let context = app("test", "alice", "room-a");
        let source = room_source("alice", "room-b");
        let mut registry = root.registry();
        registry.register_context(&context).unwrap();
        registry.add_sources(&context, [source.clone()]).unwrap();
        let active = registry.context_epoch(&context).unwrap();
        assert_ne!(active, 0);
        assert_eq!(registry.retained_contexts().unwrap(), registry.contexts().unwrap());
        registry.remove_context(&context);
        assert!(registry.contexts().unwrap().is_empty());
        let retained = registry.retained_contexts().unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].epoch, 0);
        assert!(retained[0].label.contains(&source));
        assert!(registry.ensure_context_epoch(&context, 0).is_err());
        assert!(registry.ensure_allowed_for_activation(&context, active, &room_recipient("alice", "room-b")).is_err());
        drop(registry);
        let mut restored = root.registry();
        assert!(restored.contexts().unwrap().is_empty());
        assert_eq!(restored.retained_contexts().unwrap(), retained);
        restored.register_context(&context).unwrap();
        assert!(restored.ensure_context_epoch(&context, 0).is_err());
        assert_ne!(restored.context_epoch(&context).unwrap(), active);
        assert_eq!(restored.retained_contexts().unwrap()[0].label, retained[0].label);
    }

    #[test]
    fn retired_legacy_storage_does_not_taint_a_fresh_compartment() {
        let root = TestRoot::new();
        let storage = root.0.join("app_data/test");
        fs::create_dir_all(&storage).unwrap();
        fs::write(storage.join("old-memory"), "private").unwrap();
        let mut registry = root.registry();
        let context = app("test", "alice", "room-a");
        registry.register_context(&context).unwrap();
        assert!(registry.labels(&context).unwrap().is_empty());
        assert!(registry.code_labels("test").unwrap().is_empty());
        assert!(registry.ensure_allowed(&context, &site()).is_ok());
        assert_ne!(registry.context_storage_path(&context).unwrap(), storage);
        assert_eq!(fs::read_to_string(storage.join("old-memory")).unwrap(), "private");
        let public = ContextId::PublicApp { account: "alice".into(), app: "test".into() };
        registry.register_context(&public).unwrap();
        assert!(registry.labels(&public).unwrap().is_empty());
        assert_ne!(registry.context_storage_path(&public).unwrap(), storage);
        assert_ne!(registry.context_storage_path(&public).unwrap(), registry.context_storage_path(&context).unwrap());
    }

    #[test]
    fn unrecorded_compartment_files_still_block_sharing() {
        let root = TestRoot::new();
        let context = app("test", "alice", "room-a");
        let path = storage::context_path(&root.0, &context);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("private-memory"), "private").unwrap();
        let mut registry = root.registry();
        registry.register_context(&context).unwrap();
        assert_eq!(registry.labels(&context).unwrap(), [Source::UnknownPrivate].into_iter().collect());
        assert!(registry.ensure_allowed(&context, &site()).is_err());
        let public = ContextId::PublicApp { account: "alice".into(), app: "test".into() };
        registry.register_context(&public).unwrap();
        assert!(registry.labels(&public).unwrap().is_empty());
    }

    #[test]
    fn unknown_app_source_remains_a_floor_in_every_compartment() {
        let root = TestRoot::new();
        let context = app("test", "alice", "room-a");
        let mut registry = root.registry();
        registry.register_context_with_legacy_data(&context, true).unwrap();
        assert!(registry.code_labels("test").unwrap().contains(&Source::UnknownPrivate));
        assert!(registry.ensure_allowed(&context, &site()).is_err());
        let public = ContextId::PublicApp { account: "alice".into(), app: "test".into() };
        assert!(registry.register_context(&public).is_err());
    }

    #[test]
    fn verified_stock_recovers_empty_compartments_and_protects_historical_code() {
        let root = TestRoot::new();
        let context = app("test", "alice", "room-a");
        let mut registry = root.registry();
        registry.register_context_with_legacy_data(&context, true).unwrap();
        registry.add_sources(&context, [Source::Account { account: "alice".into() }]).unwrap();
        registry.remove_context(&context);
        registry.reconcile_builtin_code("test").unwrap();
        registry.register_context(&context).unwrap();
        assert!(registry.labels(&context).unwrap().is_empty());
        assert!(registry.influences(&context).unwrap().is_empty());
        assert!(registry.ensure_allowed(&context, &site()).is_ok());
        assert!(registry.code_labels("test").unwrap().contains(&Source::UnknownPrivate));
        assert!(registry.code_influences("test").unwrap().contains(&Influence::Unknown));
        assert!(registry.ensure_labels_allowed(&registry.code_labels("test").unwrap(), &site()).is_err());
        let public = ContextId::PublicApp { account: "alice".into(), app: "test".into() };
        registry.register_context(&public).unwrap();
        assert!(registry.labels(&public).unwrap().is_empty());
        registry.restore_app_code_provenance("test").unwrap();
        assert!(registry.labels(&context).unwrap().contains(&Source::UnknownPrivate));
        assert!(registry.labels(&public).is_err(), "restoring private history must invalidate public execution");
        drop(registry);
        let mut restored = root.registry();
        restored.register_context(&context).unwrap();
        assert!(restored.labels(&context).unwrap().contains(&Source::UnknownPrivate));
        assert!(restored.register_context(&public).is_err());
    }

    #[test]
    fn stock_replacement_never_clears_running_memory_or_saved_files() {
        let root = TestRoot::new();
        let live = app("test", "alice", "room-a");
        let saved = app("test", "alice", "room-b");
        let public = ContextId::PublicApp { account: "alice".into(), app: "test".into() };
        let mut registry = root.registry();
        for context in [&live, &saved, &public] { registry.register_context(context).unwrap(); }
        let source = room_source("alice", "room-b");
        registry.add_sources(&live, [source.clone()]).unwrap();
        registry.add_code_sources("test", [source.clone()]).unwrap();
        registry.add_code_influences("test", [Influence::Unknown]).unwrap();
        for context in [&saved, &public] {
            let path = storage::context_path(&root.0, context);
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("saved"), "private").unwrap();
            registry.remove_context(context);
        }
        let epoch = registry.context_epoch(&live).unwrap();
        registry.reconcile_builtin_code("test").unwrap();
        assert_eq!(registry.context_epoch(&live).unwrap(), epoch);
        assert_eq!(registry.labels(&live).unwrap(), [source.clone()].into());
        assert!(registry.influences(&live).unwrap().contains(&Influence::Unknown));
        assert!(registry.ensure_allowed(&live, &site()).is_err());
        registry.register_context(&saved).unwrap();
        assert_eq!(registry.labels(&saved).unwrap(), [source.clone()].into());
        assert!(registry.ensure_allowed(&saved, &site()).is_err());
        assert!(registry.register_context(&public).is_err());
        for context in [&saved, &public] {
            assert_eq!(fs::read_to_string(storage::context_path(&root.0, context).join("saved")).unwrap(), "private");
        }
        drop(registry);
        let mut restored = root.registry();
        assert!(restored.register_context(&public).is_err());
        restored.register_context(&saved).unwrap();
        assert_eq!(restored.labels(&saved).unwrap(), [source].into());
        let fresh = app("test", "alice", "room-c");
        restored.register_context(&fresh).unwrap();
        assert!(restored.labels(&fresh).unwrap().is_empty());
    }

    #[test]
    fn schema_two_without_a_history_archive_keeps_its_code_floor() {
        let root = TestRoot::new();
        let context = app("test", "alice", "room-a");
        let mut registry = root.registry();
        registry.register_context_with_legacy_data(&context, true).unwrap();
        drop(registry);
        let path = root.0.join(METADATA_FILE);
        let mut old: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        old.as_object_mut().unwrap().remove("historical_code");
        fs::write(path, serde_json::to_vec(&old).unwrap()).unwrap();
        let mut restored = root.registry();
        restored.register_context(&context).unwrap();
        assert!(restored.labels(&context).unwrap().contains(&Source::UnknownPrivate));
        assert!(restored.ensure_allowed(&context, &site()).is_err());
    }

    #[test]
    fn unrecorded_private_source_cannot_reuse_a_verified_stock_floor() {
        let root = TestRoot::new();
        let context = app("test", "alice", "room-a");
        let source = room_source("alice", "room-b");
        let mut registry = root.registry();
        registry.register_context(&context).unwrap();
        registry.add_code_sources("test", [source.clone()]).unwrap();
        registry.remove_context(&context);
        registry.reconcile_builtin_code("test").unwrap();
        registry.register_context(&context).unwrap();
        assert!(registry.labels(&context).unwrap().is_empty());
        let epoch = registry.context_epoch(&context).unwrap();
        registry.register_context_with_legacy_data(&context, true).unwrap();
        assert_eq!(registry.context_epoch(&context).unwrap(), epoch);
        let label = registry.labels(&context).unwrap();
        assert!(label.contains(&source));
        assert!(label.contains(&Source::UnknownPrivate));
        assert!(registry.influences(&context).unwrap().contains(&Influence::Unknown));
        assert!(registry.ensure_allowed(&context, &site()).is_err());
        registry.remove_context(&context);
        registry.register_context_with_legacy_data(&context, true).unwrap();
        assert_eq!(registry.labels(&context).unwrap(), label);
        let public = ContextId::PublicApp { account: "alice".into(), app: "test".into() };
        assert!(registry.register_context(&public).is_err());
    }

    #[test]
    fn an_unrecorded_nonstock_source_without_history_is_unknown() {
        let root = TestRoot::new();
        let context = app("test", "alice", "room-a");
        let mut registry = root.registry();
        registry.reconcile_builtin_code("test").unwrap();
        registry.register_context_with_legacy_data(&context, true).unwrap();
        assert_eq!(registry.labels(&context).unwrap(), [Source::UnknownPrivate].into());
        assert_eq!(registry.influences(&context).unwrap(), [Influence::Unknown].into());
        assert!(registry.ensure_allowed(&context, &site()).is_err());
    }

    #[test]
    fn known_public_generated_source_keeps_its_recorded_influences() {
        for influence in [Influence::Model("local-provider".into()), Influence::MiniApp { account: "alice".into(), app: "generator".into() }] {
            let root = TestRoot::new();
            let context = app("test", "alice", "room-a");
            let mut registry = root.registry();
            registry.add_code_influences("test", [influence.clone()]).unwrap();
            registry.register_context_with_legacy_data(&context, true).unwrap();
            assert!(registry.labels(&context).unwrap().is_empty());
            assert_eq!(registry.influences(&context).unwrap(), [influence].into());
            registry.register_context_with_legacy_data(&context, true).unwrap();
            assert!(registry.labels(&context).unwrap().is_empty());
            assert!(registry.ensure_allowed(&context, &site()).is_ok());
        }
    }

    #[test]
    fn archived_code_requires_a_current_code_entry() {
        let root = TestRoot::new();
        let mut metadata = Metadata::default();
        metadata.historical_code.insert("test".into(), StoredProvenance::legacy(true));
        fs::write(root.0.join(METADATA_FILE), serde_json::to_vec(&metadata).unwrap()).unwrap();
        assert!(Registry::open(&root.0).is_err());
    }

    #[test]
    fn joining_unknown_code_provenance_fails_closed_while_a_listing_does_not() {
        let root = TestRoot::new();
        let storage = root.0.join("app_data/legacy");
        fs::create_dir_all(&storage).unwrap();
        fs::write(storage.join("old-memory"), "private").unwrap();
        let mut registry = root.registry();
        let receiver = agent("alice", "room-a");
        registry.register_context(&receiver).unwrap();
        // Registering the app as legacy records its code provenance as
        // UnknownPrivate (existing app_data alone no longer taints code).
        let app_context = app("legacy", "alice", "room-a");
        registry.register_context_with_legacy_data(&app_context, true).unwrap();
        assert!(registry.code_labels("legacy").unwrap().contains(&Source::UnknownPrivate));
        // A metadata listing only adds its own account source; the receiver
        // stays clean and can still reach the model provider.
        assert!(registry.labels(&receiver).unwrap().is_empty());
        assert!(registry.ensure_allowed(&receiver, &site()).is_ok());
        // A path that reads the app's code joins the unknown source fail-closed.
        registry.join_code_labels("legacy", &receiver).unwrap();
        assert!(registry.labels(&receiver).unwrap().contains(&Source::UnknownPrivate));
        assert!(registry.ensure_allowed(&receiver, &site()).is_err());
    }

    #[test]
    fn begin_agent_session_leaves_only_the_own_room_in_the_label() {
        let root = TestRoot::new();
        let mut registry = root.registry();
        let context = agent("alice", "room-a");
        registry.register_context(&context).unwrap();
        registry.add_sources(&context, [room_source("alice", "room-b"), Source::Account { account: "alice".into() }]).unwrap();
        registry.begin_agent_session(&context).unwrap();
        assert_eq!(registry.labels(&context).unwrap(), [room_source("alice", "room-a")].into_iter().collect());
        assert_eq!(registry.influences(&context).unwrap(), [Influence::RoomContent { account: "alice".into(), room: "room-a".into() }].into_iter().collect());
    }

    #[test]
    fn legacy_agent_memory_with_no_provenance_is_unknown_private() {
        let root = TestRoot::new();
        let mut registry = root.registry();
        let context = agent("alice", "room-a");
        registry.register_context_with_legacy_data(&context, true).unwrap();
        assert_eq!(registry.labels(&context).unwrap(), [Source::UnknownPrivate].into_iter().collect());
        assert!(registry.ensure_allowed(&context, &site()).is_err());
    }

    #[test]
    fn recorded_public_storage_does_not_become_unknown_on_restart() {
        let root = TestRoot::new();
        let context = app("test", "alice", "room-a");
        root.registry().register_context(&context).unwrap();
        let storage = root.0.join("app_data/test");
        fs::create_dir_all(&storage).unwrap();
        fs::write(storage.join("public-page-cache"), "public").unwrap();
        let mut restored = root.registry();
        restored.register_context(&context).unwrap();
        assert!(restored.labels(&context).unwrap().is_empty());
    }

    #[test]
    fn unknown_sources_cannot_be_released_by_policy() {
        let root = TestRoot::new();
        let mut registry = root.registry();
        let context = agent("alice", "room-a");
        registry.register_context(&context).unwrap();
        registry.add_sources(&context, [Source::UnknownPrivate]).unwrap();
        assert!(registry.set_policy(Source::UnknownPrivate, allow(site())).is_err());
        assert!(registry.set_policy(Source::UnknownPrivate, allow(Recipient::External)).is_err());
        assert!(registry.ensure_allowed(&context, &site()).is_err());
    }

    #[test]
    fn ipc_transfers_full_label_and_persists_before_delivery() {
        let root = TestRoot::new();
        let sender = agent("alice", "room-a");
        let receiver = app("tool", "bob", "room-b");
        let mut registry = root.registry();
        for context in [&sender, &receiver] { registry.register_context(context).unwrap(); }
        registry.add_sources(&sender, [room_source("alice", "room-a"), Source::Account { account: "alice".into() }]).unwrap();
        registry.add_sources(&receiver, [room_source("bob", "room-b")]).unwrap();
        registry.transfer(&sender, &receiver).unwrap();
        assert_eq!(registry.labels(&receiver).unwrap().len(), 3);
        assert_eq!(registry.labels(&sender).unwrap().len(), 2);
        let mut restored = root.registry();
        restored.register_context(&receiver).unwrap();
        assert_eq!(restored.labels(&receiver).unwrap().len(), 3);
    }

    #[test]
    fn outbound_tool_result_must_transfer_in_reverse_direction_too() {
        let root = TestRoot::new();
        let mut registry = root.registry();
        let agent = agent("alice", "room-a");
        let tool = app("tool", "alice", "room-a");
        for context in [&agent, &tool] { registry.register_context(context).unwrap(); }
        registry.add_sources(&agent, [room_source("alice", "room-a")]).unwrap();
        registry.transfer(&agent, &tool).unwrap();
        registry.add_sources(&tool, [room_source("alice", "room-b")]).unwrap();
        registry.transfer(&tool, &agent).unwrap();
        assert_eq!(registry.labels(&agent).unwrap().len(), 2);
        assert!(registry.ensure_allowed(&agent, &room_recipient("alice", "room-a")).is_err());
    }

    #[test]
    fn corrupt_or_incomplete_metadata_never_resets_to_public() {
        let root = TestRoot::new();
        for corrupt in ["{", "{}", r#"{"version":99,"policies":[],"app_labels":{},"agent_labels":[]}"#,
            r#"{"version":1,"policies":[],"app_labels":{},"agent_labels":[],"unknown":true}"#]
        {
            fs::write(root.0.join(METADATA_FILE), corrupt).unwrap();
            assert!(Registry::open(&root.0).is_err(), "accepted corrupt metadata: {corrupt}");
        }
    }

    #[test]
    fn persisted_unknown_source_allowance_is_rejected() {
        let root = TestRoot::new();
        let mut metadata = Metadata::default();
        metadata.grants.push(SharingGrant { id: 1, source: Source::UnknownPrivate, recipient: site(),
            reader: ReaderScope::AllReaders, duration: SharingDuration::Permanent });
        metadata.next_id = 2;
        fs::write(root.0.join(METADATA_FILE), serde_json::to_vec(&metadata).unwrap()).unwrap();
        assert!(Registry::open(&root.0).is_err());
    }

    #[test]
    fn failed_durable_join_blocks_delivery_and_further_output() {
        let root = TestRoot::new();
        let mut registry = root.registry();
        let context = app("test", "alice", "room-a");
        registry.register_context(&context).unwrap();
        fs::remove_file(root.0.join(METADATA_FILE)).unwrap();
        fs::create_dir(root.0.join(METADATA_FILE)).unwrap();
        assert!(registry.add_sources(&context, [room_source("alice", "room-a")]).is_err());
        assert!(registry.ensure_allowed(&context, &site()).is_err());
        assert!(registry.labels(&context).is_err());
        assert!(registry.add_sources(&context, []).is_err());
    }

    #[test]
    fn failed_initial_provenance_save_never_registers_the_context() {
        let root = TestRoot::new();
        let mut registry = root.registry();
        fs::create_dir(root.0.join(METADATA_FILE)).unwrap();
        let context = app("test", "alice", "room-a");
        assert!(registry.register_context(&context).is_err());
        assert!(registry.ensure_allowed(&context, &site()).is_err());
    }

    #[test]
    fn recipient_scopes_do_not_confuse_hosts_ports_schemes_or_models() {
        let root = TestRoot::new();
        let mut registry = root.registry();
        let context = agent("alice", "room-a");
        let source = room_source("alice", "room-a");
        registry.register_context(&context).unwrap();
        registry.add_sources(&context, [source.clone()]).unwrap();
        registry.set_policy(source, allow(site())).unwrap();
        assert_eq!(site(), Recipient::network_origin("https://EXAMPLE.COM:443/other?query=1").unwrap());
        for destination in ["http://example.com", "https://example.com:8443", "https://child.example.com", "https://example.com.evil.test"] {
            assert!(registry.ensure_allowed(&context, &Recipient::network_origin(destination).unwrap()).is_err());
        }
        assert!(registry.ensure_allowed(&context, &Recipient::ModelProvider("https://example.com".into())).is_err());
        assert!(Recipient::network_origin("https://user:secret@example.com").is_err());
        assert!(Recipient::network_origin("file:///tmp/private").is_err());
    }

    #[test]
    fn app_identity_cannot_escape_provenance_storage() {
        let root = TestRoot::new();
        let mut registry = root.registry();
        for id in ["", "..", ".", "../other", "/tmp/outside", "apps/other"] {
            assert!(registry.register_context(&app(id, "alice", "room-a")).is_err());
        }
    }

    #[test]
    fn metadata_lives_outside_app_controlled_storage() {
        let root = TestRoot::new();
        let mut registry = root.registry();
        registry.register_context(&app("test", "alice", "room-a")).unwrap();
        assert!(root.0.join(METADATA_FILE).is_file());
        assert!(!root.0.join("app_data/test").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(root.0.join(METADATA_FILE)).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}

#[cfg(test)]
mod phase2_tests;

#[cfg(test)]
mod exact_action_tests;
