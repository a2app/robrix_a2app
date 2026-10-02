//! Upfront task permission requests: the resolved plan an AI room's agent
//! asks for before a task, and the atomic batch that applies the user's
//! answer.
//!
//! The agent writes a plain-language explanation and lists structured needs in
//! Robrix's own vocabulary (catalog ids, room ids from `list_rooms`, URLs and
//! the names of tools a mini-app registered). Robrix turns that request into a
//! [`TaskPlan`] of exact, checkable items — grants plus the information-flow
//! rules they imply — shows it once, and applies the approved subset with the
//! grant primitives that already exist. The request is untrusted input: the
//! agent never supplies a grant record, and Robrix never grants more than the
//! user approved.
//!
//! This module deliberately has no UI and no runtime state. The runtime is the
//! only place that knows which rooms are joined and who the model provider is;
//! it passes those in. Everything here is unit-testable.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::capabilities::{self, Capability, Risk, Scope};
use crate::capabilities::{FlowOutput, FlowSource};
use crate::information_flow::{
    self as flow, ContextId, ReaderScope, Recipient, SharingDuration, Source,
};
use crate::permissions::{
    Effective, GrantDuration, NetworkScope, Permission, PermissionContext, PermissionStore,
    RoomScope,
};

/// The most needs one request may carry, so a prompt stays readable.
pub const MAX_NEEDS: usize = 24;
/// The most room/space targets a single need may name.
pub const MAX_TARGETS_PER_NEED: usize = 32;
/// The longest agent-written explanation shown on the prompt.
pub const MAX_EXPLANATION_CHARS: usize = 600;
/// The longest per-need "why", shown as the agent's own words.
pub const MAX_WHY_CHARS: usize = 160;
/// The longest task title kept for the turn-card receipt chip.
pub const MAX_TITLE_CHARS: usize = 120;

/// The capability ids the four default directory tools use. Their reads are
/// granted when an AI room's session starts (unless the user denied them), so
/// the agent can name real rooms in a request without asking first. Directory
/// results carry no message content but are still other people's words.
pub const DIRECTORY_CAP_IDS: &[&str] = &[
    "matrix.rooms.list",
    "matrix.spaces.list",
    "matrix.space.info.read",
    "matrix.space.rooms.list",
];

/// One structured need the agent asks for. `id` is the agent's own short label
/// (`n1`, `n2`); Robrix echoes it back in the result so the model can tell
/// which items were granted.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskNeed {
    /// A catalog capability, optionally narrowed to room/space targets.
    Capability {
        id: String,
        capability: String,
        #[serde(default)]
        targets: Vec<String>,
        #[serde(default)]
        why: Option<String>,
    },
    /// One exact HTTP(S) URL the agent wants to fetch.
    Website { id: String, url: String, #[serde(default)] why: Option<String> },
    /// A tool a mini-app registered in this room, by the name the model sees.
    AppTool { id: String, tool: String, #[serde(default)] why: Option<String> },
}

impl TaskNeed {
    pub fn id(&self) -> &str {
        match self {
            Self::Capability { id, .. } | Self::Website { id, .. } | Self::AppTool { id, .. } => id,
        }
    }

    fn why(&self) -> Option<&str> {
        match self {
            Self::Capability { why, .. } | Self::Website { why, .. } | Self::AppTool { why, .. } => why.as_deref(),
        }
    }
}

/// The raw request the agent's `request_task_permissions` tool receives.
#[derive(Clone, Debug, Deserialize)]
pub struct TaskRequest {
    pub task: String,
    #[serde(default)]
    pub explanation: String,
    pub needs: Vec<TaskNeed>,
}

/// Validates a request's shape and lengths, returning a message the model can
/// act on. Called before any prompt opens, so a malformed request never costs
/// the user attention.
pub fn parse_request(value: &serde_json::Value) -> Result<TaskRequest, String> {
    let request: TaskRequest = serde_json::from_value(value.clone())
        .map_err(|error| format!("Malformed permission request: {error}. Send `task`, an optional `explanation`, and a `needs` array."))?;
    validate_request(&request)?;
    Ok(request)
}

pub fn validate_request(request: &TaskRequest) -> Result<(), String> {
    if request.task.trim().is_empty() {
        return Err("`task` must not be empty; it names the task for the user.".into());
    }
    if request.needs.is_empty() {
        return Err("`needs` must list at least one thing the task needs.".into());
    }
    if request.needs.len() > MAX_NEEDS {
        return Err(format!("A request may list at most {MAX_NEEDS} needs; split the task or narrow it."));
    }
    if request.explanation.chars().count() > MAX_EXPLANATION_CHARS {
        return Err(format!("`explanation` is limited to {MAX_EXPLANATION_CHARS} characters."));
    }
    let mut ids = BTreeSet::new();
    for need in &request.needs {
        let id = need.id().trim();
        if id.is_empty() {
            return Err("Every need needs a non-empty `id`.".into());
        }
        if !ids.insert(id.to_string()) {
            return Err(format!("Duplicate need id `{id}`."));
        }
        if let Some(why) = need.why()
            && why.chars().count() > MAX_WHY_CHARS
        {
            return Err(format!("`why` for `{id}` is limited to {MAX_WHY_CHARS} characters."));
        }
        if let TaskNeed::Capability { targets, .. } = need
            && targets.len() > MAX_TARGETS_PER_NEED
        {
            return Err(format!("`{id}` names more than {MAX_TARGETS_PER_NEED} targets."));
        }
    }
    Ok(())
}

/// Where a plan item came from: the agent's own list, or a rule Robrix derived
/// and tied back to the needs that caused it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItemOrigin {
    Requested,
    Implied { because: Vec<String> },
}

/// The closed set of reasons an item was not granted, returned to the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskReason {
    Declined,
    BlockedByRoomPolicy,
    NotOffered,
    InvalidTarget,
    AlreadyAllowed,
}

impl TaskReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Declined => "declined",
            Self::BlockedByRoomPolicy => "blocked_by_room_policy",
            Self::NotOffered => "not_offered",
            Self::InvalidTarget => "invalid_target",
            Self::AlreadyAllowed => "already_allowed",
        }
    }
}

/// What applying (or declining) an item would do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItemState {
    AlreadyAllowed,
    NeedsGrant,
    Blocked(TaskReason),
    NotOffered(TaskReason),
}

impl ItemState {
    pub fn chip(&self) -> &'static str {
        match self {
            Self::AlreadyAllowed => "Already allowed",
            Self::NeedsGrant => "Will be granted",
            Self::Blocked(_) => "Blocked",
            Self::NotOffered(_) => "Not offered",
        }
    }

    pub fn is_grantable(&self) -> bool {
        matches!(self, Self::NeedsGrant)
    }
}

/// The exact operation a plan item authorizes. Robrix builds every stored
/// grant and sharing rule from this; the agent never supplies one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanAction {
    /// A catalog capability, narrowed to a room/space selection. `permission`
    /// is the group's kebab-case id so the plan serializes without exposing a
    /// non-serde enum; apply parses it back with [`Permission::from_str`].
    Scoped { permission: String, capability: String, scope: RoomScope },
    /// One exact HTTP(S) URL, scoped to the rooms the AI room belongs to.
    Network { url: String, scope: RoomScope },
    /// A tool a mini-app registered in this room.
    Tool { tool: String, scope: RoomScope },
    /// An information-flow rule implied by the requested reads and outputs.
    Flow { source: Source, recipient: Recipient },
}

impl PlanAction {
    /// The human title for the Details row.
    pub fn title(&self) -> String {
        match self {
            Self::Scoped { capability, .. } => capabilities::by_id(capability)
                .map(|c| c.title.to_string())
                .unwrap_or_else(|| capability.clone()),
            Self::Network { url, .. } => format!("Open {url}"),
            Self::Tool { tool, .. } => format!("Use the mini-app tool \"{tool}\""),
            Self::Flow { source, recipient } => format!("Let {} reach {}", flow_source_label(source), flow_recipient_label(recipient)),
        }
    }

    /// The exact, checkable detail line: ids, capability id, URL.
    pub fn detail(&self) -> Option<String> {
        match self {
            Self::Scoped { capability, scope, .. } => Some(match scope {
                RoomScope::AllRooms => format!("{capability} · all rooms"),
                RoomScope::Selection { rooms, spaces } => {
                    let mut parts = Vec::new();
                    if !rooms.is_empty() {
                        parts.push(format!("rooms: {}", rooms.join(", ")));
                    }
                    if !spaces.is_empty() {
                        parts.push(format!("spaces: {}", spaces.join(", ")));
                    }
                    format!("{capability} · {}", parts.join(" · "))
                }
            }),
            Self::Network { url, .. } => Some(url.clone()),
            Self::Tool { tool, .. } => Some(tool.clone()),
            Self::Flow { source, recipient } => Some(format!("{} → {}", flow_source_label(source), flow_recipient_label(recipient))),
        }
    }

    /// The scope a permission or network grant for this item is recorded under.
    pub fn scope(&self) -> Option<&RoomScope> {
        match self {
            Self::Scoped { scope, .. } | Self::Network { scope, .. } | Self::Tool { scope, .. } => Some(scope),
            Self::Flow { .. } => None,
        }
    }
}

fn flow_source_label(source: &Source) -> String {
    match source {
        Source::Account { account } => format!("your account ({account})"),
        Source::Room { room, .. } => format!("room {room}"),
        Source::RoomDirectory { .. } => "your room directory".to_string(),
        Source::UnknownPrivate => "stored data".to_string(),
    }
}

fn flow_recipient_label(recipient: &Recipient) -> String {
    match recipient {
        Recipient::NetworkOrigin(origin) => origin.clone(),
        Recipient::ModelProvider(provider) => format!("your AI service ({provider})"),
        Recipient::MatrixRoom { room, .. } => format!("room {room}"),
        Recipient::External => "outside Robrix".to_string(),
        Recipient::Clipboard => "the clipboard".to_string(),
    }
}

/// One row of the resolved plan shown in the prompt's Details list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanItem {
    pub id: String,
    pub origin: ItemOrigin,
    pub action: PlanAction,
    pub state: ItemState,
    pub why: Option<String>,
    pub risk: Risk,
}

impl PlanItem {
    /// The reasons an unapproved item was not granted.
    pub fn not_granted_reason(&self) -> Option<TaskReason> {
        match &self.state {
            ItemState::AlreadyAllowed => Some(TaskReason::AlreadyAllowed),
            ItemState::Blocked(reason) | ItemState::NotOffered(reason) => Some(*reason),
            ItemState::NeedsGrant => Some(TaskReason::Declined),
        }
    }
}

/// The resolved plan: what the user sees, identified by `plan_hash` so Robrix
/// applies exactly what was displayed and never a re-resolved plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskPlan {
    pub task_id: u64,
    pub subject: String,
    pub context: ContextId,
    pub epoch: u64,
    pub title: String,
    pub explanation: String,
    pub items: Vec<PlanItem>,
    /// SHA-256 over the displayed plan. Recomputed by the runtime before
    /// applying; a mismatch means the plan changed under the modal.
    pub plan_hash: [u8; 32],
}

impl TaskPlan {
    /// Every item that needs the user's approval to be granted.
    pub fn grantable(&self) -> impl Iterator<Item = &PlanItem> {
        self.items.iter().filter(|item| item.state.is_grantable())
    }

    /// Whether anything at all needs the user's attention. When false no modal
    /// opens and the tool answers at once.
    pub fn needs_prompt(&self) -> bool {
        self.grantable().next().is_some()
    }

    /// Recomputes and stores the canonical hash. Called once at the end of
    /// resolution.
    pub fn seal(&mut self) {
        self.plan_hash = self.compute_hash();
    }

    /// The hash of exactly the fields the user saw. Independent of map order.
    pub fn compute_hash(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(self.task_id.to_le_bytes());
        hasher.update(self.subject.as_bytes());
        hasher.update(b"\0");
        hasher.update(serde_json::to_vec(&self.context).unwrap_or_default());
        hasher.update(self.epoch.to_le_bytes());
        hasher.update(self.title.as_bytes());
        hasher.update(b"\0");
        hasher.update(self.explanation.as_bytes());
        for item in &self.items {
            hasher.update(b"\0item\0");
            hasher.update(item.id.as_bytes());
            hasher.update(serde_json::to_vec(&item.origin).unwrap_or_default());
            hasher.update(serde_json::to_vec(&item.action).unwrap_or_default());
            hasher.update(serde_json::to_vec(&item.state).unwrap_or_default());
            hasher.update(item.why.as_deref().unwrap_or("").as_bytes());
            hasher.update([item.risk as u8]);
        }
        hasher.finalize().into()
    }
}

/// Looks up whether an information-flow rule already exists, without joining
/// any new source. The runtime supplies the global registry; tests supply a
/// fake.
pub trait FlowLookup {
    fn already_allowed(&self, source: &Source, recipient: &Recipient, reader: &ContextId) -> bool;
    /// Every source the context already knows. The plan derives a flow rule
    /// for each of them to each requested recipient, so an approved output is
    /// not refused later just because the agent's label had already grown
    /// (e.g. the room directory it listed in order to name a target).
    fn context_sources(&self, _context: &ContextId) -> Vec<Source> { Vec::new() }
}

/// The production lookup, backed by the process-wide information-flow registry.
pub struct GlobalFlow;

impl FlowLookup for GlobalFlow {
    fn already_allowed(&self, source: &Source, recipient: &Recipient, reader: &ContextId) -> bool {
        flow::sharing_allows_for_reader(source, recipient, reader).unwrap_or(false)
    }

    fn context_sources(&self, context: &ContextId) -> Vec<Source> {
        flow::labels(context).map(|label| label.into_iter().collect()).unwrap_or_default()
    }
}

/// Everything the resolver needs that only the runtime knows.
pub struct ResolveInputs<'a> {
    pub task_id: u64,
    pub subject: &'a str,
    pub account: &'a str,
    /// The AI room whose agent is asking.
    pub room: &'a str,
    pub context: &'a ContextId,
    pub epoch: u64,
    /// The model endpoint the agent's requests actually leave through. `None`
    /// when no provider is configured, which skips provider flow items.
    pub model_recipient: Option<Recipient>,
    /// The account's homeserver origin. AI replies and activity rows are
    /// unencrypted Matrix state, so every source the agent read must also be
    /// allowed to reach this origin. `None` when it cannot be resolved.
    pub homeserver_recipient: Option<Recipient>,
    /// Whether a named room/space is one the user is joined to.
    pub joined: &'a dyn Fn(&str) -> bool,
    /// The capability ids this session offers; anything else is not offered.
    pub declared_capabilities: &'a [&'a str],
    pub store: &'a PermissionStore,
    pub flow: &'a dyn FlowLookup,
}

/// Turns the agent's request into the exact plan the user will see. Never
/// errors on a need: a bad item becomes `NotOffered` or `Blocked`, so the rest
/// of the plan still reaches the user and the model learns what failed.
pub fn resolve(request: &TaskRequest, inputs: &ResolveInputs<'_>) -> Result<TaskPlan, String> {
    validate_request(request)?;
    let mut items: Vec<PlanItem> = Vec::new();
    // (need id, source) pairs that must reach each output / the provider.
    let mut read_sources: Vec<(String, Source)> = Vec::new();
    // (need id, recipient) for outputs the agent named.
    let mut outputs: Vec<(String, Recipient)> = Vec::new();
    // The AI room itself is already a source (its messages reach the agent).
    let room_source = Source::Room { account: inputs.account.into(), room: inputs.room.into() };

    for need in &request.needs {
        match need {
            TaskNeed::Capability { id, capability, targets, why } => {
                let why = cleaned_why(why.as_deref());
                let Some(cap) = capabilities::by_id(capability) else {
                    items.push(item(id, why, Risk::Low, ItemState::NotOffered(TaskReason::NotOffered),
                        PlanAction::Scoped { permission: Permission::MatrixRoomInfo.as_str().to_string(), capability: capability.clone(), scope: RoomScope::room(inputs.room) }));
                    continue;
                };
                if !inputs.declared_capabilities.contains(&cap.id) {
                    items.push(item(id, why, cap.risk, ItemState::NotOffered(TaskReason::NotOffered), scoped_action(cap, &[inputs.room.to_string()])));
                    continue;
                }
                let Some(group) = cap.group else {
                    items.push(item(id, why, cap.risk, ItemState::NotOffered(TaskReason::NotOffered), scoped_action(cap, &[inputs.room.to_string()])));
                    continue;
                };
                let selection = match capability_selection(cap, targets, inputs.room) {
                    Ok(selection) => selection,
                    Err(reason) => {
                        items.push(item(id, why, cap.risk, ItemState::NotOffered(reason), scoped_action(cap, &[inputs.room.to_string()])));
                        continue;
                    }
                };
                let all_targets = selection.all_ids();
                if all_targets.iter().any(|target| !(inputs.joined)(target)) {
                    items.push(item(id, why, cap.risk, ItemState::NotOffered(TaskReason::InvalidTarget), scoped_action_for(group, cap, selection.room_scope())));
                    continue;
                }
                // A hard room/space block always wins; the item is shown as
                // blocked with Robrix's own reason, never granted.
                let mut blocked = None;
                for target in all_targets {
                    let context = PermissionContext { origin_room: Some(inputs.room), target_room: Some(target) };
                    if let Some((access, evaluation)) = inputs.store.capability_room_evaluation(cap, context)
                        && evaluation.decision == crate::permissions::PolicyDecision::Deny
                    {
                        blocked = Some(evaluation.reason.public_message(access));
                        break;
                    }
                }
                let action = scoped_action_for(group, cap, selection.room_scope());
                if let Some(reason) = blocked {
                    items.push(PlanItem { id: id.clone(), origin: ItemOrigin::Requested, action, state: ItemState::Blocked(TaskReason::BlockedByRoomPolicy), why, risk: cap.risk });
                    let _ = reason; // the public message is rendered by the UI from the policy evaluation
                    continue;
                }
                // Already granted at every target means the user sees nothing.
                let declares_perm = |p: Permission| inputs.declared_capabilities.iter().any(|id| capabilities::by_id(id).and_then(|c| c.group) == Some(p));
                let declares_cap = |c: &Capability| inputs.declared_capabilities.contains(&c.id);
                let already = !all_targets.is_empty() && all_targets.iter().all(|target| {
                    let context = PermissionContext { origin_room: Some(inputs.room), target_room: Some(target) };
                    inputs.store.effective_capability_for_in_context(inputs.subject, &declares_perm, &declares_cap, cap, context) == Effective::Granted
                });
                let state = if already { ItemState::AlreadyAllowed } else { ItemState::NeedsGrant };
                items.push(PlanItem { id: id.clone(), origin: ItemOrigin::Requested, action, state, why, risk: cap.risk });
                collect_contract_flow(cap, inputs, all_targets, id, &mut read_sources, &mut outputs);
            }
            TaskNeed::Website { id, url, why } => {
                let why = cleaned_why(why.as_deref());
                match NetworkScope::from_url(url, crate::permissions::NetworkScopeKind::ExactUrl) {
                    Ok(scope) => {
                        let url = match scope {
                            NetworkScope::ExactUrl(url) => url,
                            other => other.normalized().map(|scope| match scope {
                                NetworkScope::ExactUrl(url) => url,
                                _ => unreachable!(),
                            }).unwrap_or_else(|_| url.clone()),
                        };
                        let already = inputs.store.is_url_allowed(inputs.subject, &url,
                            PermissionContext { origin_room: Some(inputs.room), target_room: Some(inputs.room) });
                        let state = if already { ItemState::AlreadyAllowed } else { ItemState::NeedsGrant };
                        items.push(PlanItem { id: id.clone(), origin: ItemOrigin::Requested,
                            action: PlanAction::Network { url: url.clone(), scope: RoomScope::room(inputs.room) },
                            state, why, risk: Risk::High });
                        if let Ok(recipient) = Recipient::network_origin(&url) {
                            outputs.push((id.clone(), recipient));
                        }
                    }
                    Err(_) => {
                        items.push(PlanItem { id: id.clone(), origin: ItemOrigin::Requested,
                            action: PlanAction::Network { url: url.clone(), scope: RoomScope::room(inputs.room) },
                            state: ItemState::NotOffered(TaskReason::InvalidTarget), why, risk: Risk::High });
                    }
                }
            }
            TaskNeed::AppTool { id, tool, why } => {
                let why = cleaned_why(why.as_deref());
                let tool = tool.trim().to_string();
                let exists = inputs.declared_capabilities.iter().any(|_| true) && !tool.is_empty();
                // The name is validated against the room's live app tools by
                // the runtime before apply; an empty name is never valid.
                let state = if !exists { ItemState::NotOffered(TaskReason::InvalidTarget) } else { ItemState::NeedsGrant };
                items.push(PlanItem { id: id.clone(), origin: ItemOrigin::Requested,
                    action: PlanAction::Tool { tool, scope: RoomScope::room(inputs.room) }, state, why, risk: Risk::High });
            }
        }
    }

    // Derive the information-flow rules Robrix requires: every source that
    // will be read must be allowed to reach the model provider and every
    // output the agent named. One item per (source, recipient) pair, carrying
    // the needs that caused it.
    let mut flow_items: BTreeMap<(Source, Recipient), Vec<String>> = BTreeMap::new();
    let mut sources: Vec<Source> = vec![room_source];
    // Include every source the agent already holds, not just the ones this
    // request names: the output check requires every source in the label to
    // permit the recipient, so a directory read performed to find the target
    // would otherwise veto the very fetch/post the user just approved.
    for source in inputs.flow.context_sources(inputs.context) {
        if !sources.contains(&source) {
            sources.push(source);
        }
    }
    for (_, source) in &read_sources {
        if !sources.contains(source) {
            sources.push(source.clone());
        }
    }
    // The agent writes every turn's reply (and its live activity rows) back
    // into its own room, and those rows are unencrypted Matrix state that the
    // homeserver stores. Derive a rule for EVERY source the agent holds — not
    // just the reads this request names — to reach both the room and the
    // homeserver, exactly as the provider rules below do: the whole-label
    // output check would otherwise refuse the reply the moment the label grows
    // (a read, a directory listing, an app-tool transfer) and the user would
    // never see the answer. Each rule is attributed to the read need that
    // produced its source, so unchecking that read drops its own rules.
    let own_room = Recipient::MatrixRoom { account: inputs.account.into(), room: inputs.room.into() };
    let mut reply_recipients = vec![own_room];
    if let Some(homeserver) = &inputs.homeserver_recipient {
        reply_recipients.push(homeserver.clone());
    }
    for recipient in &reply_recipients {
        for source in &sources {
            let entry = flow_items.entry((source.clone(), recipient.clone())).or_default();
            for (need_id, read) in &read_sources {
                if read == source && !entry.contains(need_id) {
                    entry.push(need_id.clone());
                }
            }
        }
    }
    if let Some(provider) = &inputs.model_recipient {
        for source in &sources {
            let entry = flow_items.entry((source.clone(), provider.clone())).or_default();
            for (need_id, read) in &read_sources {
                if read == source && !entry.contains(need_id) {
                    entry.push(need_id.clone());
                }
            }
        }
    }
    for (need_id, recipient) in &outputs {
        for source in &sources {
            let entry = flow_items.entry((source.clone(), recipient.clone())).or_default();
            for id in std::iter::once(need_id).chain(read_sources.iter().filter(|(_, read)| read == source).map(|(read_id, _)| read_id)) {
                if !entry.contains(id) {
                    entry.push(id.clone());
                }
            }
        }
    }
    for ((source, recipient), mut because) in flow_items {
        because.sort();
        because.dedup();
        let already = inputs.flow.already_allowed(&source, &recipient, inputs.context);
        let state = if already { ItemState::AlreadyAllowed } else { ItemState::NeedsGrant };
        let risk = because.iter().filter_map(|id| request.needs.iter().find(|need| need.id() == id))
            .map(need_risk).max().unwrap_or_else(|| match &source {
                Source::Room { .. } => Risk::High,
                _ => Risk::Medium,
            });
        let origin = ItemOrigin::Implied { because };
        items.push(PlanItem { id: format!("flow:{}:{}", item_key(&source), item_key(&recipient)), origin,
            action: PlanAction::Flow { source, recipient }, state, why: None, risk });
    }

    let mut plan = TaskPlan {
        task_id: inputs.task_id,
        subject: inputs.subject.to_string(),
        context: inputs.context.clone(),
        epoch: inputs.epoch,
        title: clip(&request.task, MAX_TITLE_CHARS),
        explanation: clean_text(&request.explanation, MAX_EXPLANATION_CHARS),
        items,
        plan_hash: [0; 32],
    };
    plan.seal();
    Ok(plan)
}

fn need_risk(need: &TaskNeed) -> Risk {
    match need {
        TaskNeed::Capability { capability, .. } => capabilities::by_id(capability).map(|c| c.risk).unwrap_or(Risk::Medium),
        TaskNeed::Website { .. } | TaskNeed::AppTool { .. } => Risk::High,
    }
}

fn item_key(value: &impl Serialize) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn item(id: &str, why: Option<String>, risk: Risk, state: ItemState, action: PlanAction) -> PlanItem {
    PlanItem { id: id.to_string(), origin: ItemOrigin::Requested, action, state, why, risk }
}

fn scoped_action(cap: &Capability, rooms: &[String]) -> PlanAction {
    let permission = cap.group.unwrap_or(Permission::MatrixRoomInfo).as_str().to_string();
    PlanAction::Scoped { permission, capability: cap.id.to_string(),
        scope: RoomScope::Selection { rooms: rooms.to_vec(), spaces: Vec::new() } }
}

fn scoped_action_for(permission: Permission, cap: &Capability, scope: RoomScope) -> PlanAction {
    PlanAction::Scoped { permission: permission.as_str().to_string(), capability: cap.id.to_string(), scope }
}

fn cleaned_why(why: Option<&str>) -> Option<String> {
    let why = why?;
    let cleaned = clean_text(why, MAX_WHY_CHARS);
    (!cleaned.is_empty()).then_some(cleaned)
}

/// Strips control characters and caps the length of agent-authored text shown
/// verbatim in the UI.
pub fn clean_text(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max));
    for ch in text.chars() {
        if ch == '\n' || ch == '\t' || !ch.is_control() {
            out.push(ch);
        }
        if out.chars().count() >= max {
            break;
        }
    }
    out.trim().to_string()
}

fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// The room/space selection a capability need resolves to. Attached-room
/// capabilities with no targets mean this AI room; multi-room and space
/// capabilities need at least one target.
struct Selection {
    rooms: Vec<String>,
    spaces: Vec<String>,
    all: Vec<String>,
}

impl Selection {
    fn all_ids(&self) -> &[String] {
        &self.all
    }

    fn room_scope(&self) -> RoomScope {
        RoomScope::Selection { rooms: self.rooms.clone(), spaces: self.spaces.clone() }
    }
}

fn capability_selection(cap: &Capability, targets: &[String], room: &str) -> Result<Selection, TaskReason> {
    let mut rooms = Vec::new();
    let mut spaces = Vec::new();
    for target in targets {
        let target = target.trim();
        if target.is_empty() {
            continue;
        }
        if cap.scope == Scope::Space {
            spaces.push(target.to_string());
        } else {
            rooms.push(target.to_string());
        }
    }
    if rooms.is_empty() && spaces.is_empty() {
        if matches!(cap.scope, Scope::Room | Scope::Instance) {
            rooms.push(room.to_string());
        } else {
            return Err(TaskReason::InvalidTarget);
        }
    }
    rooms.sort();
    rooms.dedup();
    spaces.sort();
    spaces.dedup();
    let mut all = rooms.clone();
    all.extend(spaces.iter().cloned());
    Ok(Selection { rooms, spaces, all })
}

/// Records the implied flow caused by one capability: its source(s) for reads
/// and its recipient(s) for outputs, using the catalog's explicit contract.
fn collect_contract_flow(
    cap: &Capability,
    inputs: &ResolveInputs<'_>,
    targets: &[String],
    need_id: &str,
    read_sources: &mut Vec<(String, Source)>,
    outputs: &mut Vec<(String, Recipient)>,
) {
    let Some(contract) = cap.flow_contract() else { return };
    let mut sources: Vec<Source> = Vec::new();
    match contract.source {
        FlowSource::TargetRoom => {
            for target in targets {
                sources.push(Source::Room { account: inputs.account.into(), room: target.clone() });
                // A read's query — the room id and any pagination params — is
                // sent to that room's server, so the context's label must be
                // allowed to reach it, exactly as `ensure_room_output` checks
                // before the read. Without this the approved read is refused
                // the moment it arrives.
                outputs.push((need_id.to_string(), Recipient::MatrixRoom { account: inputs.account.into(), room: target.clone() }));
            }
        }
        FlowSource::AttachedRoom => sources.push(Source::Room { account: inputs.account.into(), room: inputs.room.into() }),
        FlowSource::Account | FlowSource::InstalledAppCode | FlowSource::IpcAppCode => {
            if DIRECTORY_CAP_IDS.contains(&cap.id) {
                sources.push(Source::RoomDirectory { account: inputs.account.into() });
            } else {
                sources.push(Source::Account { account: inputs.account.into() });
            }
        }
        FlowSource::None | FlowSource::Peer => {}
    }
    for source in sources {
        read_sources.push((need_id.to_string(), source));
    }
    match contract.output {
        FlowOutput::TargetRoom => {
            for target in targets {
                outputs.push((need_id.to_string(), Recipient::MatrixRoom { account: inputs.account.into(), room: target.clone() }));
            }
        }
        // A network output is the website need's own item; peer routes carry
        // labels by transfer rather than a sharing rule.
        FlowOutput::External | FlowOutput::Network | FlowOutput::MatrixServer
        | FlowOutput::MatrixSearch | FlowOutput::MatrixPagination | FlowOutput::Local
        | FlowOutput::Browser | FlowOutput::Clipboard
        | FlowOutput::Peer(_) => {}
    }
}

/// The output of applying an approved plan, kept so the runtime can revoke
/// exactly these grants when the turn closes or the user asks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedTask {
    pub task_id: u64,
    pub subject: String,
    pub context: ContextId,
    pub epoch: u64,
    pub title: String,
    pub plan_hash: [u8; 32],
    pub grants: Vec<GrantRef>,
}

/// One stored grant created by a task, addressed by its kind so revoke can
/// use the right primitive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantRef {
    Scoped(u64),
    Network(u64),
    Tool(u64),
    Flow(u64),
}

/// Information-flow operations the applier needs. The production impl talks to
/// the global registry; tests use a fake to prove rollback without touching
/// disk.
pub trait FlowApply {
    fn ensure_epoch(&self, context: &ContextId, epoch: u64) -> Result<(), String>;
    fn grant(&self, source: Source, recipient: Recipient, reader: ReaderScope, duration: SharingDuration) -> Result<u64, String>;
    fn revoke(&self, id: u64) -> Result<bool, String>;
}

pub struct GlobalFlowApply;

impl FlowApply for GlobalFlowApply {
    fn ensure_epoch(&self, context: &ContextId, epoch: u64) -> Result<(), String> {
        flow::ensure_context_epoch(context, epoch)
    }

    fn grant(&self, source: Source, recipient: Recipient, reader: ReaderScope, duration: SharingDuration) -> Result<u64, String> {
        flow::grant_sharing(source, recipient, reader, duration)
    }

    fn revoke(&self, id: u64) -> Result<bool, String> {
        flow::revoke_sharing(id)
    }
}

/// Why an approved plan could not be applied. The runtime reports the failing
/// item to the model; the plan is rolled back whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyError {
    StaleContext,
    Failed { item: String, reason: String },
}

impl ApplyError {
    pub fn message(&self) -> String {
        match self {
            Self::StaleContext => "The requesting context stopped or restarted.".to_string(),
            Self::Failed { item, reason } => format!("Could not apply `{item}`: {reason}"),
        }
    }
}

/// Applies every approved item as one atomic batch: permission, network and
/// tool grants first, information-flow rules last. Any failure rolls back
/// everything already applied and returns the failing item, so a task never
/// leaves a half-granted state.
pub fn apply(
    plan: &TaskPlan,
    approved: &BTreeSet<String>,
    store: &mut PermissionStore,
    flow: &dyn FlowApply,
) -> Result<AppliedTask, ApplyError> {
    if plan.compute_hash() != plan.plan_hash {
        return Err(ApplyError::Failed { item: "plan".into(), reason: "The plan changed after it was shown.".into() });
    }
    let mut applied = AppliedTask {
        task_id: plan.task_id,
        subject: plan.subject.clone(),
        context: plan.context.clone(),
        epoch: plan.epoch,
        title: plan.title.clone(),
        plan_hash: plan.plan_hash,
        grants: Vec::new(),
    };
    let origin_room = Some(plan.context.room().unwrap_or_default());
    let rollback = |applied: &AppliedTask, store: &mut PermissionStore, flow: &dyn FlowApply| {
        for grant in applied.grants.iter().rev() {
            match *grant {
                GrantRef::Scoped(id) | GrantRef::Tool(id) => { store.remove_scoped_grant(id); }
                GrantRef::Network(id) => { store.remove_network_grant(id); }
                GrantRef::Flow(id) => { let _ = flow.revoke(id); }
            }
        }
    };
    for item in plan.items.iter().filter(|item| approved.contains(&item.id) && item.state.is_grantable()) {
        let result = match &item.action {
            PlanAction::Scoped { permission, capability, scope } => {
                match Permission::from_str(permission) {
                    None => Err("The plan names an unknown permission group.".to_string()),
                    Some(permission) => store.grant_scoped(&plan.subject, permission, Some(capability), scope.clone(), GrantDuration::RobrixSession, origin_room)
                        .map(GrantRef::Scoped),
                }
            }
            PlanAction::Network { url, scope } => store
                .allow_network(&plan.subject, NetworkScope::ExactUrl(url.clone()), scope.clone(), GrantDuration::RobrixSession, origin_room)
                .map(GrantRef::Network),
            PlanAction::Tool { tool, scope } => store
                .grant_scoped_tool(&plan.subject, tool, "", scope.clone(), GrantDuration::RobrixSession, origin_room)
                .map(GrantRef::Tool),
            PlanAction::Flow { source, recipient } => {
                let (account, room) = match (&plan.context, plan.context.room()) {
                    (ContextId::Agent { account, .. }, Some(room)) => (account.clone(), room.to_string()),
                    _ => { rollback(&applied, store, flow); return Err(ApplyError::StaleContext); }
                };
                // The plan carries the context activation; a restarted context
                // is refused exactly as the guarded transport refuses it.
                if let Err(reason) = flow.ensure_epoch(&plan.context, plan.epoch) {
                    rollback(&applied, store, flow);
                    return Err(ApplyError::Failed { item: item.id.clone(), reason });
                }
                flow.grant(source.clone(), recipient.clone(), ReaderScope::Context(plan.context.clone()),
                    SharingDuration::RoomSession { account, room })
                    .map(GrantRef::Flow)
            }
        };
        match result {
            Ok(grant) => applied.grants.push(grant),
            Err(reason) => {
                rollback(&applied, store, flow);
                return Err(ApplyError::Failed { item: item.id.clone(), reason });
            }
        }
    }
    Ok(applied)
}

/// Removes every grant an applied task created. Safe to call more than once;
/// a grant already revoked by the user is simply not found.
pub fn rollback(applied: &AppliedTask, store: &mut PermissionStore, flow: &dyn FlowApply) {
    for grant in applied.grants.iter().rev() {
        match *grant {
            GrantRef::Scoped(id) | GrantRef::Tool(id) => { store.remove_scoped_grant(id); }
            GrantRef::Network(id) => { store.remove_network_grant(id); }
            GrantRef::Flow(id) => { let _ = flow.revoke(id); }
        }
    }
}

/// Narrows an approved set so an unchecked read drops the implied flow rules
/// that depend on it. A flow item whose `because` names an unapproved need is
/// removed, repeatedly, because removing one can orphan another.
pub fn dependent_approval(plan: &TaskPlan, ids: impl IntoIterator<Item = String>) -> BTreeSet<String> {
    let mut approved: BTreeSet<String> = ids.into_iter().collect();
    loop {
        let mut removed = false;
        for item in &plan.items {
            if !approved.contains(&item.id) {
                continue;
            }
            if let ItemOrigin::Implied { because } = &item.origin
                && !because.is_empty()
                && !because.iter().all(|id| approved.contains(id))
            {
                approved.remove(&item.id);
                removed = true;
            }
        }
        if !removed {
            break;
        }
    }
    approved
}

/// The per-task ledger the runtime keeps so `close_active_turn`, teardown and
/// the AI panel's Revoke can drop exactly what a task applied.
#[derive(Default)]
pub struct TaskLedger {
    tasks: BTreeMap<u64, AppliedTask>,
}

impl TaskLedger {
    pub fn insert(&mut self, applied: AppliedTask) {
        self.tasks.insert(applied.task_id, applied);
    }

    pub fn remove(&mut self, task_id: u64) -> Option<AppliedTask> {
        self.tasks.remove(&task_id)
    }

    pub fn get(&self, task_id: u64) -> Option<&AppliedTask> {
        self.tasks.get(&task_id)
    }

    pub fn tasks(&self) -> impl Iterator<Item = &AppliedTask> {
        self.tasks.values()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Drops and returns every task, for session teardown.
    pub fn drain(&mut self) -> Vec<AppliedTask> {
        std::mem::take(&mut self.tasks).into_values().collect()
    }
}

/// The result the tool hands back to the model. Reasons are the closed set in
/// [`TaskReason`].
pub fn outcome(plan: &TaskPlan, approved: &BTreeSet<String>) -> serde_json::Value {
    let mut granted = Vec::new();
    let mut not_granted = Vec::new();
    for item in &plan.items {
        if approved.contains(&item.id) && item.state.is_grantable() {
            granted.push(serde_json::Value::String(item.id.clone()));
        } else if let Some(reason) = item.not_granted_reason() {
            not_granted.push(serde_json::json!({ "id": item.id, "reason": reason.as_str() }));
        }
    }
    let status = if not_granted.is_empty() && !granted.is_empty() { "granted" }
        else if granted.is_empty() { "declined" }
        else { "partial" };
    serde_json::json!({
        "task_id": plan.task_id,
        "status": status,
        "granted": granted,
        "not_granted": not_granted,
        "lasts": "until this turn ends",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::{PolicyDecision, RoomAccess};

    struct FakeFlow {
        allowed: std::cell::RefCell<BTreeSet<(String, String)>>,
        revoked: std::cell::RefCell<Vec<u64>>,
        next: std::cell::Cell<u64>,
        fail_next: std::cell::Cell<bool>,
    }

    impl FakeFlow {
        fn new() -> Self {
            Self { allowed: Default::default(), revoked: Default::default(), next: std::cell::Cell::new(1), fail_next: std::cell::Cell::new(false) }
        }
    }

    impl FlowApply for FakeFlow {
        fn ensure_epoch(&self, _context: &ContextId, _epoch: u64) -> Result<(), String> { Ok(()) }
        fn grant(&self, source: Source, recipient: Recipient, _reader: ReaderScope, _duration: SharingDuration) -> Result<u64, String> {
            if self.fail_next.replace(false) { return Err("flow store unavailable".into()); }
            let id = self.next.get();
            self.next.set(id + 1);
            self.allowed.borrow_mut().insert((item_key(&source), item_key(&recipient)));
            Ok(id)
        }
        fn revoke(&self, id: u64) -> Result<bool, String> {
            self.revoked.borrow_mut().push(id);
            Ok(true)
        }
    }

    struct FakeLookup {
        allowed: BTreeSet<(String, String)>,
    }

    impl FlowLookup for FakeLookup {
        fn already_allowed(&self, source: &Source, recipient: &Recipient, _reader: &ContextId) -> bool {
            self.allowed.contains(&(item_key(source), item_key(recipient)))
        }
    }

    /// A lookup whose context already holds sources, as it would after an
    /// earlier read in the same turn.
    struct GrownLookup {
        grown: Vec<Source>,
    }

    impl FlowLookup for GrownLookup {
        fn already_allowed(&self, _source: &Source, _recipient: &Recipient, _reader: &ContextId) -> bool {
            false
        }
        fn context_sources(&self, _context: &ContextId) -> Vec<Source> {
            self.grown.clone()
        }
    }

    const DECLARED: &[&str] = &[
        "matrix.room.messages.read",
        "matrix.rooms.messages.read",
        "matrix.rooms.message.send",
        "matrix.rooms.list",
        "matrix.spaces.list",
        "matrix.space.info.read",
        "matrix.space.rooms.list",
    ];

    fn inputs<'a>(store: &'a PermissionStore, joined: &'a dyn Fn(&str) -> bool, flow: &'a dyn FlowLookup) -> ResolveInputs<'a> {
        let context = ContextId::Agent { account: "alice".into(), room: "!ai:example.org".into() };
        // The context has to outlive `inputs`; leak a small clone so the test
        // borrow stays simple.
        let context: &'static ContextId = Box::leak(Box::new(context));
        ResolveInputs {
            task_id: 7,
            subject: "ai-room:!ai:example.org",
            account: "alice",
            room: "!ai:example.org",
            context,
            epoch: 11,
            model_recipient: Some(Recipient::ModelProvider("provider".into())),
            homeserver_recipient: Some(Recipient::network_origin("https://hs.example.org").unwrap()),
            joined,
            declared_capabilities: DECLARED,
            store,
            flow,
        }
    }

    fn request(needs: Vec<TaskNeed>) -> TaskRequest {
        TaskRequest { task: "Weekly digest of #ops".into(), explanation: "I'll read and post.".into(), needs }
    }

    #[test]
    fn a_second_request_after_the_label_grew_shows_the_new_flow_as_needs_grant() {
        let mut store = PermissionStore::default();
        store.set_matrix_write(true);
        let joined = |room: &str| matches!(room, "!ai:example.org" | "!ops:example.org");
        let grown = Source::Room { account: "alice".into(), room: "!read-earlier:example.org".into() };
        let lookup = GrownLookup { grown: vec![grown.clone()] };
        let plan = resolve(&request(vec![TaskNeed::Capability { id: "n1".into(),
            capability: "matrix.rooms.message.send".into(), targets: vec!["!ops:example.org".into()], why: None }]),
            &inputs(&store, &joined, &lookup)).unwrap();
        // The agent already read this source, so the second request must show
        // the flow it now needs (to the provider and to its own room) so the
        // user can approve the grown label rather than failing forever.
        for recipient in [Recipient::ModelProvider("provider".into()),
            Recipient::MatrixRoom { account: "alice".into(), room: "!ai:example.org".into() }] {
            let item = plan.items.iter().find(|item| matches!(&item.action, PlanAction::Flow { source, recipient: item_recipient }
                if source == &grown && item_recipient == &recipient)).expect("the grown source must produce a flow item");
            assert_eq!(item.state, ItemState::NeedsGrant);
        }
    }

    #[test]
    fn a_read_and_post_task_resolves_to_one_prompt_of_exact_items() {
        let mut store = PermissionStore::default();
        store.set_matrix_write(true);
        let joined = |room: &str| matches!(room, "!ai:example.org" | "!ops:example.org" | "!leads:example.org");
        let lookup = FakeLookup { allowed: Default::default() };
        let plan = resolve(&request(vec![
            TaskNeed::Capability { id: "n1".into(), capability: "matrix.rooms.messages.read".into(),
                targets: vec!["!ops:example.org".into()], why: Some("Read the week".into()) },
            TaskNeed::Capability { id: "n2".into(), capability: "matrix.rooms.message.send".into(),
                targets: vec!["!leads:example.org".into()], why: None },
        ]), &inputs(&store, &joined, &lookup)).unwrap();
        // Two requested items plus the derived flow rules.
        assert!(plan.items.iter().any(|i| i.id == "n1" && i.state.is_grantable()));
        assert!(plan.items.iter().any(|i| i.id == "n2" && i.state.is_grantable()));
        assert!(plan.items.iter().any(|i| matches!(i.origin, ItemOrigin::Implied { .. })));
        assert!(plan.needs_prompt());
        // What the user saw is what is hashed.
        assert_eq!(plan.plan_hash, plan.compute_hash());
    }

    #[test]
    fn a_read_derives_a_flow_rule_back_to_the_agents_own_room() {
        let store = PermissionStore::default();
        let joined = |room: &str| matches!(room, "!ai:example.org" | "!ops:example.org");
        let lookup = FakeLookup { allowed: Default::default() };
        let plan = resolve(&request(vec![TaskNeed::Capability { id: "n1".into(),
            capability: "matrix.rooms.messages.read".into(), targets: vec!["!ops:example.org".into()], why: None }]),
            &inputs(&store, &joined, &lookup)).unwrap();
        let own_room = Recipient::MatrixRoom { account: "alice".into(), room: "!ai:example.org".into() };
        let read_source = Source::Room { account: "alice".into(), room: "!ops:example.org".into() };
        let item = plan
            .items
            .iter()
            .find(|item| matches!(&item.action, PlanAction::Flow { source, recipient }
                if source == &read_source && recipient == &own_room))
            .expect("the agent replies in its own room, so a read must be allowed to reach it");
        assert!(item.state.is_grantable(), "the user must approve the read reaching the AI room");
        // The reply is unencrypted Matrix state, so it must also be allowed to
        // reach the homeserver that stores it.
        let homeserver = Recipient::network_origin("https://hs.example.org").unwrap();
        let item = plan
            .items
            .iter()
            .find(|item| matches!(&item.action, PlanAction::Flow { source, recipient }
                if source == &read_source && recipient == &homeserver))
            .expect("a read written into unencrypted state must be allowed to the homeserver");
        assert!(item.state.is_grantable(), "the user must approve the read reaching the homeserver");
    }

    #[test]
    fn a_blocked_room_is_shown_blocked_and_never_grantable() {
        let mut store = PermissionStore::default();
        store.set_room_policy("!ops:example.org", RoomAccess::Read, PolicyDecision::Deny);
        let joined = |room: &str| matches!(room, "!ai:example.org" | "!ops:example.org");
        let lookup = FakeLookup { allowed: Default::default() };
        let plan = resolve(&request(vec![TaskNeed::Capability { id: "n1".into(),
            capability: "matrix.rooms.messages.read".into(), targets: vec!["!ops:example.org".into()], why: None }]),
            &inputs(&store, &joined, &lookup)).unwrap();
        let item = plan.items.iter().find(|i| i.id == "n1").unwrap();
        assert_eq!(item.state, ItemState::Blocked(TaskReason::BlockedByRoomPolicy));
        assert!(!item.state.is_grantable());
        assert_eq!(item.not_granted_reason(), Some(TaskReason::BlockedByRoomPolicy));
    }

    #[test]
    fn an_already_allowed_item_costs_the_user_no_attention() {
        let mut store = PermissionStore::default();
        store.set("ai-room:!ai:example.org", Permission::MatrixRoomsRead, crate::permissions::GrantState::Granted);
        store.grant_scoped("ai-room:!ai:example.org", Permission::MatrixRoomsRead, Some("matrix.rooms.messages.read"),
            RoomScope::room("!ops:example.org"), GrantDuration::RobrixSession, Some("!ai:example.org")).unwrap();
        let joined = |room: &str| matches!(room, "!ai:example.org" | "!ops:example.org");
        let lookup = FakeLookup { allowed: Default::default() };
        let plan = resolve(&request(vec![TaskNeed::Capability { id: "n1".into(),
            capability: "matrix.rooms.messages.read".into(), targets: vec!["!ops:example.org".into()], why: None }]),
            &inputs(&store, &joined, &lookup)).unwrap();
        assert_eq!(plan.items.iter().find(|i| i.id == "n1").unwrap().state, ItemState::AlreadyAllowed);
    }

    #[test]
    fn unjoined_targets_are_invalid_and_unknown_capabilities_are_not_offered() {
        let store = PermissionStore::default();
        let joined = |room: &str| room == "!ai:example.org";
        let lookup = FakeLookup { allowed: Default::default() };
        let plan = resolve(&request(vec![
            TaskNeed::Capability { id: "n1".into(), capability: "matrix.rooms.messages.read".into(), targets: vec!["!nope:example.org".into()], why: None },
            TaskNeed::Capability { id: "n2".into(), capability: "network.http".into(), targets: vec![], why: None },
        ]), &inputs(&store, &joined, &lookup)).unwrap();
        assert_eq!(plan.items.iter().find(|i| i.id == "n1").unwrap().state, ItemState::NotOffered(TaskReason::InvalidTarget));
        assert_eq!(plan.items.iter().find(|i| i.id == "n2").unwrap().state, ItemState::NotOffered(TaskReason::NotOffered));
    }

    #[test]
    fn a_website_need_names_the_exact_url_and_derives_a_flow_to_its_origin() {
        let store = PermissionStore::default();
        let joined = |room: &str| room == "!ai:example.org";
        let lookup = FakeLookup { allowed: Default::default() };
        let plan = resolve(&request(vec![TaskNeed::Website { id: "w1".into(),
            url: "https://status.example.com/page".into(), why: None }]), &inputs(&store, &joined, &lookup)).unwrap();
        let item = plan.items.iter().find(|i| i.id == "w1").unwrap();
        assert_eq!(item.action, PlanAction::Network { url: "https://status.example.com/page".into(), scope: RoomScope::room("!ai:example.org") });
        assert!(plan.items.iter().any(|i| matches!(&i.action, PlanAction::Flow { recipient: Recipient::NetworkOrigin(origin), .. } if origin == "https://status.example.com")));
    }

    #[test]
    fn apply_is_atomic_and_rolls_back_everything_on_a_late_failure() {
        let mut store = PermissionStore::default();
        store.set_matrix_write(true);
        let joined = |room: &str| matches!(room, "!ai:example.org" | "!leads:example.org");
        let lookup = FakeLookup { allowed: Default::default() };
        let plan = resolve(&request(vec![TaskNeed::Capability { id: "n1".into(),
            capability: "matrix.rooms.message.send".into(), targets: vec!["!leads:example.org".into()], why: None }]),
            &inputs(&store, &joined, &lookup)).unwrap();
        let approved: BTreeSet<String> = plan.items.iter().map(|i| i.id.clone()).collect();
        let flow = FakeFlow::new();
        // The permission grant applies first, then its flow rule fails; the
        // permission grant must be rolled back so nothing half-applies.
        flow.fail_next.set(true);
        let result = apply(&plan, &approved, &mut store, &flow);
        assert!(result.is_err());
        let scoped = store.scoped_grants(&plan.subject);
        assert!(scoped.is_empty(), "permission grants must be rolled back when the batch fails");
    }

    #[test]
    fn apply_then_rollback_removes_scoped_network_and_flow_grants() {
        let mut store = PermissionStore::default();
        store.set_matrix_write(true);
        let joined = |room: &str| matches!(room, "!ai:example.org" | "!leads:example.org");
        let lookup = FakeLookup { allowed: Default::default() };
        let plan = resolve(&request(vec![
            TaskNeed::Capability { id: "n1".into(), capability: "matrix.rooms.message.send".into(), targets: vec!["!leads:example.org".into()], why: None },
            TaskNeed::Website { id: "w1".into(), url: "https://status.example.com/".into(), why: None },
        ]), &inputs(&store, &joined, &lookup)).unwrap();
        let approved: BTreeSet<String> = plan.items.iter().map(|i| i.id.clone()).collect();
        let flow = FakeFlow::new();
        let applied = apply(&plan, &approved, &mut store, &flow).unwrap();
        assert!(!applied.grants.is_empty());
        assert_eq!(store.scoped_grants(&plan.subject).len(), 1);
        assert_eq!(store.network_grants(&plan.subject).len(), 1);
        rollback(&applied, &mut store, &flow);
        assert!(store.scoped_grants(&plan.subject).is_empty());
        assert!(store.network_grants(&plan.subject).is_empty());
        assert!(!flow.revoked.borrow().is_empty());
    }

    #[test]
    fn the_plan_hash_detects_a_changed_plan() {
        let mut plan = TaskPlan { task_id: 1, subject: "s".into(), context: ContextId::Agent { account: "a".into(), room: "r".into() },
            epoch: 1, title: "t".into(), explanation: "e".into(), items: Vec::new(), plan_hash: [0; 32] };
        plan.seal();
        let hash = plan.plan_hash;
        plan.title.push('!');
        assert_ne!(hash, plan.compute_hash());
        let error = apply(&plan, &BTreeSet::new(), &mut PermissionStore::default(), &FakeFlow::new());
        assert_eq!(error, Err(ApplyError::Failed { item: "plan".into(), reason: "The plan changed after it was shown.".into() }));
    }

    #[test]
    fn malformed_and_oversized_requests_are_refused_before_any_prompt() {
        assert!(parse_request(&serde_json::json!({ "task": "x", "needs": [] })).is_err());
        assert!(parse_request(&serde_json::json!({ "task": "x", "explanation": "e",
            "needs": [{ "id": "n", "kind": "capability", "capability": "matrix.rooms.list" }] })).is_ok());
        let too_many = (0..MAX_NEEDS + 1).map(|i| serde_json::json!({ "id": format!("n{i}"), "kind": "website", "url": "https://e.com/" })).collect::<Vec<_>>();
        assert!(parse_request(&serde_json::json!({ "task": "x", "needs": too_many })).is_err());
        assert!(parse_request(&serde_json::json!({ "task": "", "needs": [{ "id": "n", "kind": "website", "url": "https://e.com/" }] })).is_err());
    }

    #[test]
    fn cleaning_strips_control_characters_and_caps_length() {
        let cleaned = clean_text("hi\u{7}there", 4);
        assert_eq!(cleaned, "hith");
        assert!(!clean_text("a\nb", 10).contains('\u{7}'));
    }

    #[test]
    fn outcome_reports_closed_reasons_for_unapproved_items() {
        let store = PermissionStore::default();
        let joined = |room: &str| matches!(room, "!ai:example.org" | "!ops:example.org");
        let mut store = store;
        store.set_room_policy("!ops:example.org", RoomAccess::Read, PolicyDecision::Deny);
        let lookup = FakeLookup { allowed: Default::default() };
        let plan = resolve(&request(vec![TaskNeed::Capability { id: "n1".into(),
            capability: "matrix.rooms.messages.read".into(), targets: vec!["!ops:example.org".into()], why: None }]),
            &inputs(&store, &joined, &lookup)).unwrap();
        let value = outcome(&plan, &BTreeSet::new());
        assert_eq!(value["task_id"], 7);
        let reasons: Vec<&str> = value["not_granted"].as_array().unwrap().iter()
            .filter_map(|entry| entry["reason"].as_str()).collect();
        assert!(reasons.contains(&"blocked_by_room_policy"));
        assert_eq!(value["lasts"], "until this turn ends");
    }

    #[test]
    fn an_applied_task_needs_no_further_prompt() {
        // A two-room read-and-post task resolves to one prompt; once approved,
        // the same task resolves with nothing left to ask.
        let mut store = PermissionStore::default();
        store.set_matrix_write(true);
        let joined = |room: &str| matches!(room, "!ai:example.org" | "!ops:example.org" | "!leads:example.org");
        let lookup = FakeLookup { allowed: Default::default() };
        let needs = || vec![
            TaskNeed::Capability { id: "n1".into(), capability: "matrix.rooms.messages.read".into(), targets: vec!["!ops:example.org".into()], why: None },
            TaskNeed::Capability { id: "n2".into(), capability: "matrix.rooms.message.send".into(), targets: vec!["!leads:example.org".into()], why: None },
        ];
        let first = resolve(&request(needs()), &inputs(&store, &joined, &lookup)).unwrap();
        assert!(first.needs_prompt());
        let approved: BTreeSet<String> = first.items.iter().map(|i| i.id.clone()).collect();
        let flow = FakeFlow::new();
        let applied = apply(&first, &approved, &mut store, &flow).unwrap();
        assert!(!applied.grants.is_empty());
        // The flow rules the apply created are now already allowed, so a
        // re-resolution has nothing left to ask for.
        let applied_lookup = FakeLookup { allowed: flow.allowed.borrow().clone() };
        let second = resolve(&request(needs()), &inputs(&store, &joined, &applied_lookup)).unwrap();
        assert!(!second.needs_prompt(), "the approved task must not prompt again");
        assert!(second.items.iter().all(|item| !item.state.is_grantable()));
    }

    #[test]
    fn unchecking_a_read_drops_the_flow_rules_that_depend_on_it() {
        let plan = TaskPlan {
            task_id: 1, subject: "s".into(), context: ContextId::Agent { account: "a".into(), room: "r".into() },
            epoch: 1, title: "t".into(), explanation: "e".into(), plan_hash: [0; 32],
            items: vec![
                PlanItem { id: "n1".into(), origin: ItemOrigin::Requested, risk: Risk::High, why: None,
                    action: PlanAction::Network { url: "https://example.com/".into(), scope: RoomScope::room("r") }, state: ItemState::NeedsGrant },
                PlanItem { id: "flow:1".into(), origin: ItemOrigin::Implied { because: vec!["n1".into()] }, risk: Risk::High, why: None,
                    action: PlanAction::Flow { source: Source::Room { account: "a".into(), room: "r".into() },
                        recipient: Recipient::network_origin("https://example.com/").unwrap() }, state: ItemState::NeedsGrant },
            ],
        };
        let approved = dependent_approval(&plan, Vec::new());
        assert!(approved.is_empty());
        let approved = dependent_approval(&plan, vec!["n1".into(), "flow:1".into()]);
        assert!(approved.contains("flow:1"));
        let approved = dependent_approval(&plan, vec!["flow:1".into()]);
        assert!(!approved.contains("flow:1"), "a flow without its cause must not be applied");
    }

    #[test]
    fn ledger_tracks_and_drains_tasks() {
        let mut ledger = TaskLedger::default();
        ledger.insert(AppliedTask { task_id: 3, subject: "s".into(), context: ContextId::Agent { account: "a".into(), room: "r".into() },
            epoch: 1, title: "t".into(), plan_hash: [0; 32], grants: vec![GrantRef::Scoped(2)] });
        assert_eq!(ledger.tasks().count(), 1);
        assert!(ledger.remove(3).is_some());
        assert!(ledger.is_empty());
        ledger.insert(AppliedTask { task_id: 4, subject: "s".into(), context: ContextId::Agent { account: "a".into(), room: "r".into() },
            epoch: 1, title: "t".into(), plan_hash: [0; 32], grants: vec![] });
        assert_eq!(ledger.drain().len(), 1);
    }
}
