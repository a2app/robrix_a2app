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
    Effective, GrantDuration, GrantState, NetworkScope, Permission, PermissionContext, PermissionStore,
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
        if id.starts_with("flow:") {
            return Err("Need ids beginning with `flow:` are reserved by Robrix; use a short label such as `n1`.".into());
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
    /// The read was approved, but one of the information-flow rules it needs
    /// was left unchecked, so the data cannot actually reach the agent.
    DeclinedDependency,
    BlockedByRoomPolicy,
    BlockedByPermission,
    NotOffered,
    InvalidTarget,
    AlreadyAllowed,
}

impl TaskReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Declined => "declined",
            Self::DeclinedDependency => "declined_dependency",
            Self::BlockedByRoomPolicy => "blocked_by_room_policy",
            Self::BlockedByPermission => "blocked_by_permission",
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
            Self::Tool { tool, .. } => Some(if tool.is_empty() { "Unnamed mini-app tool".to_string() } else { tool.clone() }),
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
    /// Host-derived dependencies for each implied sharing row. Sources that
    /// are already in memory cannot disappear when a read is unchecked.
    #[serde(default)]
    pub flow_dependencies: BTreeMap<String, FlowDependencies>,
    /// SHA-256 over the displayed plan. Recomputed by the runtime before
    /// applying; a mismatch means the plan changed under the modal.
    pub plan_hash: [u8; 32],
    /// SHA-256 over the resolved items' exact action and origin kind only,
    /// independent of `task_id`, item ids, `why`, `title`, `explanation`,
    /// `epoch` and `state`. Two requests for the same needs share this even
    /// when the agent renames its need labels, so "Not now" sticks.
    pub needs_fingerprint: [u8; 32],
}

/// A flow is needed when its source is held (or at least one selected read
/// introduces it) and its recipient is used (or is part of every reply).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowDependencies {
    pub source_already_held: bool,
    pub source_needs: Vec<String>,
    pub recipient_always: bool,
    pub recipient_needs: Vec<String>,
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

    /// Recomputes and stores the canonical hashes. Called once at the end of
    /// resolution.
    pub fn seal(&mut self) {
        self.plan_hash = self.compute_hash();
        self.needs_fingerprint = self.compute_needs_fingerprint();
    }

    /// The stable identity of what this plan asks for, ignoring the agent's
    /// labels: one key per item from its exact action and whether it was
    /// requested or implied. Implied `because` ids are intentionally excluded
    /// so a renamed need does not defeat a dismissal.
    pub fn need_keys(&self) -> BTreeSet<String> {
        self.items
            .iter()
            .map(|item| format!("{}|{}", item_key(&item.action), match &item.origin {
                ItemOrigin::Requested => "requested",
                ItemOrigin::Implied { .. } => "implied",
            }))
            .collect()
    }

    /// The fingerprint keyed into the per-room "Not now" memory.
    pub fn compute_needs_fingerprint(&self) -> [u8; 32] {
        let mut needs: Vec<String> = self.need_keys().into_iter().collect();
        needs.sort();
        let mut hasher = Sha256::new();
        for need in needs {
            hasher.update(need.as_bytes());
            hasher.update(b"\0");
        }
        hasher.finalize().into()
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
        hasher.update(serde_json::to_vec(&self.flow_dependencies).unwrap_or_default());
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
    /// Resolves a full or unambiguous raw tool name to its canonical full name
    /// in this room. Missing, foreign-room and ambiguous tools return `None`.
    pub app_tool_name: &'a dyn Fn(&str) -> Option<String>,
    pub store: &'a PermissionStore,
    pub flow: &'a dyn FlowLookup,
}

/// Shared by resolution and the live recheck: an explicit denial cannot be
/// replaced by a scoped grant, and a mixed selection needs every target.
fn capability_state_at_all(
    store: &PermissionStore,
    subject: &str,
    declared: &BTreeSet<&str>,
    cap: &Capability,
    targets: &[String],
    origin_room: Option<&str>,
) -> ItemState {
    let declares_perm = |p: Permission| declared.iter().any(|id| capabilities::by_id(id).and_then(|c| c.group) == Some(p));
    let declares_cap = |c: &Capability| declared.contains(c.id);
    if targets.is_empty() {
        return ItemState::NotOffered(TaskReason::InvalidTarget);
    }
    let mut needs_grant = false;
    for target in targets {
        let context = PermissionContext { origin_room, target_room: Some(target.as_str()) };
        if let Some((_, evaluation)) = store.capability_room_evaluation(cap, context)
            && evaluation.decision == crate::permissions::PolicyDecision::Deny
        {
            return ItemState::Blocked(TaskReason::BlockedByRoomPolicy);
        }
        match store.effective_capability_for_in_context(subject, declares_perm, declares_cap, cap, context) {
            Effective::Granted => {}
            Effective::NeedsPrompt => needs_grant = true,
            Effective::Denied => return ItemState::Blocked(TaskReason::BlockedByPermission),
            Effective::Undeclared => return ItemState::NotOffered(TaskReason::NotOffered),
        }
    }
    if needs_grant { ItemState::NeedsGrant } else { ItemState::AlreadyAllowed }
}

fn network_state(store: &PermissionStore, subject: &str, url: &str, context: PermissionContext<'_>) -> ItemState {
    if store.is_restricted(subject) || store.state(subject, Permission::Network) == GrantState::Denied
        || store.capability_state(subject, "network.http") == GrantState::Denied
    {
        ItemState::Blocked(TaskReason::BlockedByPermission)
    } else if store.is_url_allowed(subject, url, context) {
        ItemState::AlreadyAllowed
    } else {
        ItemState::NeedsGrant
    }
}

fn tool_state(store: &PermissionStore, subject: &str, tool: &str, context: PermissionContext<'_>) -> ItemState {
    let effective = store.tool_effective(subject, tool, None);
    if store.state(subject, Permission::McpTools) == GrantState::Denied || effective == Effective::Denied {
        ItemState::Blocked(TaskReason::BlockedByPermission)
    } else if effective == Effective::Granted || store.has_scoped_tool_grant(subject, tool, "", context) {
        ItemState::AlreadyAllowed
    } else {
        ItemState::NeedsGrant
    }
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
    let declared: BTreeSet<&str> = inputs.declared_capabilities.iter().copied().collect();

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
                let action = scoped_action_for(group, cap, selection.room_scope());
                let state = capability_state_at_all(inputs.store, inputs.subject, &declared, cap, all_targets, Some(inputs.room));
                let usable = matches!(state, ItemState::AlreadyAllowed | ItemState::NeedsGrant);
                items.push(PlanItem { id: id.clone(), origin: ItemOrigin::Requested, action, state, why, risk: cap.risk });
                if usable {
                    collect_contract_flow(cap, inputs, all_targets, id, &mut read_sources, &mut outputs);
                }
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
                        let state = network_state(inputs.store, inputs.subject, &url,
                            PermissionContext { origin_room: Some(inputs.room), target_room: Some(inputs.room) });
                        let usable = matches!(state, ItemState::AlreadyAllowed | ItemState::NeedsGrant);
                        items.push(PlanItem { id: id.clone(), origin: ItemOrigin::Requested,
                            action: PlanAction::Network { url: url.clone(), scope: RoomScope::room(inputs.room) },
                            state, why, risk: Risk::High });
                        if usable && let Ok(recipient) = Recipient::network_origin(&url) {
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
                let requested = tool.trim();
                let resolved = (!requested.is_empty()).then(|| (inputs.app_tool_name)(requested)).flatten();
                let state = match &resolved {
                    Some(tool) => tool_state(inputs.store, inputs.subject, tool,
                        PermissionContext { origin_room: Some(inputs.room), target_room: Some(inputs.room) }),
                    None => ItemState::NotOffered(TaskReason::InvalidTarget),
                };
                let tool = resolved.unwrap_or_else(|| requested.to_string());
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
    let held_sources: BTreeSet<Source> = sources.iter().cloned().collect();
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
    let mut flow_dependencies = BTreeMap::new();
    for ((source, recipient), mut because) in flow_items {
        because.sort();
        because.dedup();
        let already = inputs.flow.already_allowed(&source, &recipient, inputs.context);
        let state = if already { ItemState::AlreadyAllowed } else { ItemState::NeedsGrant };
        let risk = because.iter().filter_map(|id| request.needs.iter().find(|need| need.id() == id))
            .map(need_risk).max().unwrap_or(match &source {
                Source::Room { .. } => Risk::High,
                _ => Risk::Medium,
            });
        let id = format!("flow:{}:{}", item_key(&source), item_key(&recipient));
        flow_dependencies.insert(id.clone(), FlowDependencies {
            source_already_held: held_sources.contains(&source),
            source_needs: read_sources.iter().filter(|(_, read)| read == &source)
                .map(|(id, _)| id.clone()).collect::<BTreeSet<_>>().into_iter().collect(),
            recipient_always: reply_recipients.contains(&recipient) || inputs.model_recipient.as_ref() == Some(&recipient),
            recipient_needs: outputs.iter().filter(|(_, output)| output == &recipient)
                .map(|(id, _)| id.clone()).collect::<BTreeSet<_>>().into_iter().collect(),
        });
        let origin = ItemOrigin::Implied { because };
        items.push(PlanItem { id, origin,
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
        flow_dependencies,
        plan_hash: [0; 32],
        needs_fingerprint: [0; 32],
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
        if (ch == '\n' || ch == '\t' || !ch.is_control()) && !is_direction_control(ch) {
            out.push(ch);
        }
        if out.chars().count() >= max {
            break;
        }
    }
    out.trim().to_string()
}

/// Direction-changing format characters (`Cf`), which `char::is_control` does
/// not catch. The agent's prose is shown verbatim in the approval dialog, so a
/// bidi override could visually reorder it and spoof what the user reads.
/// Strip them from every agent-supplied string rather than render them.
fn is_direction_control(ch: char) -> bool {
    matches!(ch,
        '\u{061c}'                  // Arabic letter mark
        | '\u{200e}' | '\u{200f}'   // left-to-right / right-to-left mark
        | '\u{202a}'..='\u{202e}'   // LRE / RLE / PDF / LRO / RLO
        | '\u{2066}'..='\u{2069}'   // LRI / RLI / FSI / PDI
    )
}

fn clip(text: &str, max: usize) -> String {
    let text: String = text.chars().filter(|ch| !is_direction_control(*ch)).collect();
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
    // Upload is an account-side effect of this AI room's agent. The room
    // selected for the eventual post has its separate media-send grant.
    if cap.id == "matrix.media.upload" && targets.iter().any(|target| {
        let target = target.trim();
        !target.is_empty() && target != room
    }) { return Err(TaskReason::InvalidTarget); }
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
        if matches!(cap.scope, Scope::Room | Scope::Instance)
            || matches!(cap.id, "apps.list" | "apps.launch" | "matrix.media.upload")
        {
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
    if DIRECTORY_CAP_IDS.contains(&cap.id) {
        read_sources.push((need_id.to_string(), Source::RoomDirectory { account: inputs.account.into() }));
        return;
    }
    let mut sources: Vec<Source> = Vec::new();
    match contract.source {
        FlowSource::TargetRoom => {
            // Reading a room introduces its source; it does not send data to
            // that room. Remote queries use the homeserver rules derived above.
            for target in targets {
                sources.push(Source::Room { account: inputs.account.into(), room: target.clone() });
            }
        }
        FlowSource::AttachedRoom => sources.push(Source::Room { account: inputs.account.into(), room: inputs.room.into() }),
        FlowSource::Account | FlowSource::InstalledAppCode | FlowSource::IpcAppCode => {
            sources.push(Source::Account { account: inputs.account.into() });
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
        FlowOutput::MatrixServer if matches!(cap.id, "matrix.media.upload" | "matrix.media.download") => {
            if let Some(homeserver) = &inputs.homeserver_recipient {
                outputs.push((need_id.to_string(), homeserver.clone()));
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
    /// The re-evaluated state of items that no longer needed a grant (or could
    /// not be granted) when the batch was applied. The store can change while
    /// the modal waits, so an item `NeedsGrant` at resolve may be
    /// `AlreadyAllowed` or `Blocked` by apply time; its state is recorded here
    /// and the grant is skipped. Items absent kept their resolved state.
    pub item_states: BTreeMap<String, ItemState>,
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
    fn already_allowed(&self, source: &Source, recipient: &Recipient, reader: &ContextId) -> Result<bool, String>;
    fn grant(&self, source: Source, recipient: Recipient, reader: ReaderScope, duration: SharingDuration) -> Result<u64, String>;
    fn revoke(&self, id: u64) -> Result<bool, String>;
}

pub struct GlobalFlowApply;

impl FlowApply for GlobalFlowApply {
    fn ensure_epoch(&self, context: &ContextId, epoch: u64) -> Result<(), String> {
        flow::ensure_context_epoch(context, epoch)
    }

    fn already_allowed(&self, source: &Source, recipient: &Recipient, reader: &ContextId) -> Result<bool, String> {
        flow::sharing_allows_for_reader(source, recipient, reader)
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

/// Rechecks requested items, including disabled AlreadyAllowed rows, against
/// the current store. Flow-only carried-over rows have no permission to check.
fn recheck_item(item: &PlanItem, plan: &TaskPlan, store: &PermissionStore) -> Option<ItemState> {
    match &item.action {
        PlanAction::Scoped { permission, capability, scope } => {
            let cap = capabilities::by_id(capability)?;
            Permission::from_str(permission)?;
            let mut declared = BTreeSet::new();
            for other in &plan.items {
                if let PlanAction::Scoped { capability, .. } = &other.action {
                    declared.insert(capability.as_str());
                }
            }
            let targets: Vec<String> = match scope {
                RoomScope::AllRooms => plan.context.room().map(str::to_string).into_iter().collect(),
                RoomScope::Selection { rooms, spaces } => rooms.iter().chain(spaces).cloned().collect(),
            };
            Some(capability_state_at_all(store, &plan.subject, &declared, cap, &targets, plan.context.room()))
        }
        PlanAction::Network { url, .. } => {
            let context = PermissionContext { origin_room: plan.context.room(), target_room: plan.context.room() };
            Some(network_state(store, &plan.subject, url, context))
        }
        PlanAction::Tool { tool, .. } => Some(tool_state(store, &plan.subject, tool,
            PermissionContext { origin_room: plan.context.room(), target_room: plan.context.room() })),
        PlanAction::Flow { .. } => None,
    }
}

struct ApprovalState {
    usable: BTreeSet<String>,
    required_flows: BTreeSet<String>,
    declined_dependency: BTreeSet<String>,
}

fn flow_required(item: &PlanItem, plan: &TaskPlan, usable: &BTreeSet<String>) -> bool {
    if let Some(dependencies) = plan.flow_dependencies.get(&item.id) {
        (dependencies.source_already_held || dependencies.source_needs.iter().any(|id| usable.contains(id)))
            && (dependencies.recipient_always || dependencies.recipient_needs.iter().any(|id| usable.contains(id)))
    } else if let ItemOrigin::Implied { because } = &item.origin {
        // Host-created carried-over plans have no read/output metadata; their
        // empty causal list means the source is already in memory.
        because.iter().all(|id| usable.contains(id))
    } else {
        false
    }
}

fn requested_approval(plan: &TaskPlan, approved: &BTreeSet<String>, overrides: &BTreeMap<String, ItemState>) -> BTreeSet<String> {
    plan.items.iter().filter(|item| {
        matches!(item.origin, ItemOrigin::Requested) && match overrides.get(&item.id).unwrap_or(&item.state) {
            ItemState::AlreadyAllowed => true,
            ItemState::NeedsGrant => approved.contains(&item.id),
            _ => false,
        }
    }).map(|item| item.id.clone()).collect()
}

/// Computes the usable requested subset without weakening any sharing check.
/// A missing output rule removes that output; a missing provider/reply rule
/// removes the reads introducing its source. When that source is already
/// held, writes to the reply recipient also depend on its sharing rule.
/// Rules for an omitted read never become dependencies of an otherwise
/// usable output.
fn approval_state(plan: &TaskPlan, approved: &BTreeSet<String>, overrides: &BTreeMap<String, ItemState>) -> ApprovalState {
    let mut usable = requested_approval(plan, approved, overrides);
    let mut declined_dependency = BTreeSet::new();
    loop {
        let mut remove = BTreeSet::new();
        for item in &plan.items {
            let ItemOrigin::Implied { because } = &item.origin else { continue };
            if !flow_required(item, plan, &usable) {
                continue;
            }
            let available = match overrides.get(&item.id).unwrap_or(&item.state) {
                ItemState::AlreadyAllowed => true,
                ItemState::NeedsGrant => approved.contains(&item.id),
                _ => false,
            };
            if available { continue; }
            match plan.flow_dependencies.get(&item.id) {
                Some(dependencies) if !dependencies.recipient_always => {
                    remove.extend(dependencies.recipient_needs.iter().filter(|id| usable.contains(*id)).cloned());
                }
                Some(dependencies) => {
                    remove.extend(dependencies.source_needs.iter().filter(|id| usable.contains(*id)).cloned());
                    if dependencies.source_already_held {
                        remove.extend(dependencies.recipient_needs.iter().filter(|id| usable.contains(*id)).cloned());
                    }
                }
                None => remove.extend(because.iter().filter(|id| usable.contains(*id)).cloned()),
            }
        }
        if remove.is_empty() { break; }
        for id in remove {
            usable.remove(&id);
            declined_dependency.insert(id);
        }
    }
    let required_flows = plan.items.iter().filter(|item| flow_required(item, plan, &usable))
        .map(|item| item.id.clone()).collect();
    ApprovalState { usable, required_flows, declined_dependency }
}

/// Applies every approved item as one atomic batch: permission, network and
/// tool grants first, information-flow rules last. Any failure rolls back
/// everything already applied and returns the failing item, so a task never
/// leaves a half-granted state. Each grantable item is re-checked against the
/// live store first (see [`recheck_item`]).
pub fn apply(
    plan: &TaskPlan,
    approved: &BTreeSet<String>,
    store: &mut PermissionStore,
    flow: &dyn FlowApply,
) -> Result<AppliedTask, ApplyError> {
    if plan.compute_hash() != plan.plan_hash {
        return Err(ApplyError::Failed { item: "plan".into(), reason: "The plan changed after it was shown.".into() });
    }
    let ids: BTreeSet<&str> = plan.items.iter().map(|item| item.id.as_str()).collect();
    if ids.len() != plan.items.len() {
        return Err(ApplyError::Failed { item: "plan".into(), reason: "The plan contains duplicate item ids.".into() });
    }
    flow.ensure_epoch(&plan.context, plan.epoch).map_err(|_| ApplyError::StaleContext)?;
    let mut applied = AppliedTask {
        task_id: plan.task_id,
        subject: plan.subject.clone(),
        context: plan.context.clone(),
        epoch: plan.epoch,
        title: plan.title.clone(),
        plan_hash: plan.plan_hash,
        grants: Vec::new(),
        item_states: BTreeMap::new(),
    };
    let origin_room = Some(plan.context.room().unwrap_or_default());
    // Recheck disabled AlreadyAllowed rows too: a revoked allowance must not
    // be reported as usable, and no undisplayed replacement is granted.
    for item in &plan.items {
        if !matches!(item.state, ItemState::AlreadyAllowed | ItemState::NeedsGrant) { continue; }
        let state = if let PlanAction::Flow { source, recipient } = &item.action {
            let allowed = flow.already_allowed(source, recipient, &plan.context)
                .map_err(|reason| ApplyError::Failed { item: item.id.clone(), reason })?;
            Some(if allowed { ItemState::AlreadyAllowed } else { ItemState::NeedsGrant })
        } else {
            recheck_item(item, plan, store)
        };
        if let Some(state) = state && state != item.state {
            applied.item_states.insert(item.id.clone(), state);
        }
    }
    let approval = approval_state(plan, approved, &applied.item_states);
    for item in plan.items.iter().filter(|item| approved.contains(&item.id) && item.state.is_grantable()) {
        if matches!(item.origin, ItemOrigin::Implied { .. }) {
            if !approval.required_flows.contains(&item.id) {
                if let ItemOrigin::Implied { because } = &item.origin
                    && !because.is_empty()
                    && because.iter().all(|cause| matches!(applied.item_states.get(cause), Some(ItemState::Blocked(_))))
                {
                    let state = applied.item_states[&because[0]].clone();
                    applied.item_states.insert(item.id.clone(), state);
                }
                continue;
            }
        } else if !approval.usable.contains(&item.id) {
            continue;
        }
        if applied.item_states.get(&item.id).is_some_and(|state| !state.is_grantable()) {
            continue;
        }
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
/// a grant already revoked by the user is simply not found. A failure to
/// revoke a flow rule (or a grant that is already gone) is logged, not
/// swallowed, so a leaking rule is diagnosable. Returns false when any grant
/// could not be removed cleanly, so the caller can tell the user.
pub fn rollback(applied: &AppliedTask, store: &mut PermissionStore, flow: &dyn FlowApply) -> bool {
    let mut clean = true;
    for grant in applied.grants.iter().rev() {
        match *grant {
            GrantRef::Scoped(id) | GrantRef::Tool(id) => {
                if !store.remove_scoped_grant(id) {
                    makepad_widgets::log!("Mini-app task rollback: scoped grant {id} was already gone.");
                    clean = false;
                }
            }
            GrantRef::Network(id) => {
                if !store.remove_network_grant(id) {
                    makepad_widgets::log!("Mini-app task rollback: network grant {id} was already gone.");
                    clean = false;
                }
            }
            GrantRef::Flow(id) => {
                if let Err(error) = flow.revoke(id) {
                    makepad_widgets::log!("Mini-app task rollback: couldn't revoke flow rule {id}: {error}");
                    clean = false;
                }
            }
        }
    }
    clean
}

/// Drops sharing rows orphaned by unchecked requested needs. Keep other checked
/// rows even when a later dependency disables their cause: `apply` skips them,
/// and preserving the checked set distinguishes that skip from user refusal.
/// Existing allowances satisfy causes despite their disabled checkbox rows.
pub fn dependent_approval(plan: &TaskPlan, ids: impl IntoIterator<Item = String>) -> BTreeSet<String> {
    let approved: BTreeSet<String> = ids.into_iter().collect();
    let requested = requested_approval(plan, &approved, &BTreeMap::new());
    plan.items.iter().filter(|item| approved.contains(&item.id)
        && (matches!(item.origin, ItemOrigin::Requested) || flow_required(item, plan, &requested)))
        .map(|item| item.id.clone()).collect()
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
    outcome_with(plan, approved, &BTreeMap::new())
}

/// Like [`outcome`], but reports the item states re-evaluated when the plan
/// was applied (see [`AppliedTask::item_states`]).
pub fn outcome_of(plan: &TaskPlan, approved: &BTreeSet<String>, applied: &AppliedTask) -> serde_json::Value {
    outcome_with(plan, approved, &applied.item_states)
}

fn outcome_with(plan: &TaskPlan, approved: &BTreeSet<String>, overrides: &BTreeMap<String, ItemState>) -> serde_json::Value {
    // Which requested needs lost an implied flow row they depend on. A read
    // the user approved but whose provider/room row was left unchecked cannot
    // actually reach the agent, so it is reported as a partial need rather
    // than a clean grant. The flow ids themselves stay internal.
    let approval = approval_state(plan, approved, overrides);
    let mut flow_applied = 0usize;
    let mut flow_skipped = 0usize;
    for item in &plan.items {
        let ItemOrigin::Implied { .. } = &item.origin else { continue };
        let state = overrides.get(&item.id).unwrap_or(&item.state);
        let applied = match state {
            ItemState::AlreadyAllowed => true,
            ItemState::NeedsGrant => approved.contains(&item.id) && approval.required_flows.contains(&item.id),
            ItemState::Blocked(_) | ItemState::NotOffered(_) => false,
        };
        if applied {
            flow_applied += 1;
        } else {
            flow_skipped += 1;
        }
    }
    // Only the agent's own requested needs are listed; the implied flow rows
    // are summarized as counts so their internal ids never reach the model.
    let mut granted = Vec::new();
    let mut not_granted = Vec::new();
    let mut requested = 0usize;
    let mut satisfied = 0usize;
    let mut blocked = 0usize;
    let mut partial_needs = 0usize;
    for item in &plan.items {
        if !matches!(item.origin, ItemOrigin::Requested) {
            continue;
        }
        requested += 1;
        if approval.declined_dependency.contains(&item.id) {
            partial_needs += 1;
            not_granted.push(serde_json::json!({ "id": item.id, "reason": TaskReason::DeclinedDependency.as_str() }));
            continue;
        }
        match overrides.get(&item.id).unwrap_or(&item.state) {
            // An already-allowed item counts as satisfied: the agent can use it
            // without a new grant, so it is not a declination.
            ItemState::AlreadyAllowed => {
                satisfied += 1;
                granted.push(serde_json::Value::String(item.id.clone()));
            }
            ItemState::NeedsGrant if approved.contains(&item.id) => {
                satisfied += 1;
                granted.push(serde_json::Value::String(item.id.clone()));
            }
            ItemState::NeedsGrant => {
                not_granted.push(serde_json::json!({ "id": item.id, "reason": TaskReason::Declined.as_str() }));
            }
            ItemState::Blocked(reason) | ItemState::NotOffered(reason) => {
                blocked += 1;
                not_granted.push(serde_json::json!({ "id": item.id, "reason": reason.as_str() }));
            }
        }
    }
    // `granted`: every requested item is satisfied (granted or already
    // allowed). `blocked`: every requested item is blocked by policy or not
    // offered. `declined`: the user said no to every requested item and none
    // was blocked. `partial`: any other mix.
    let status = if satisfied == requested { "granted" }
        else if blocked == requested { "blocked" }
        else if satisfied == 0 && blocked == 0 && partial_needs == 0 { "declined" }
        else { "partial" };
    serde_json::json!({
        "task_id": plan.task_id,
        "status": status,
        "granted": granted,
        "not_granted": not_granted,
        "flow_rules": { "applied": flow_applied, "skipped": flow_skipped },
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
        stale: std::cell::Cell<bool>,
    }

    impl FakeFlow {
        fn new() -> Self {
            Self { allowed: Default::default(), revoked: Default::default(), next: std::cell::Cell::new(1), fail_next: std::cell::Cell::new(false), stale: std::cell::Cell::new(false) }
        }
    }

    impl FlowApply for FakeFlow {
        fn ensure_epoch(&self, _context: &ContextId, _epoch: u64) -> Result<(), String> {
            if self.stale.get() { Err("Context stopped".into()) } else { Ok(()) }
        }
        fn already_allowed(&self, source: &Source, recipient: &Recipient, _reader: &ContextId) -> Result<bool, String> {
            Ok(self.allowed.borrow().contains(&(item_key(source), item_key(recipient))))
        }
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
        "apps.list",
        "apps.launch",
        "matrix.media.upload",
        "matrix.media.download",
        "matrix.media.send",
        "host.composer.insert",
        "host.composer.attach",
    ];

    fn tool_name(tool: &str) -> Option<String> {
        Some(tool.to_string())
    }

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
            app_tool_name: &tool_name,
            store,
            flow,
        }
    }

    fn request(needs: Vec<TaskNeed>) -> TaskRequest {
        TaskRequest { task: "Weekly digest of #ops".into(), explanation: "I'll read and post.".into(), needs }
    }

    fn read(id: &str, room: &str) -> TaskNeed {
        TaskNeed::Capability { id: id.into(), capability: "matrix.rooms.messages.read".into(),
            targets: vec![room.into()], why: None }
    }

    fn website() -> TaskNeed {
        TaskNeed::Website { id: "website".into(), url: "https://status.example.com/page".into(), why: None }
    }

    fn checked(plan: &TaskPlan) -> Vec<String> {
        plan.grantable().map(|item| item.id.clone()).collect()
    }

    #[test]
    fn media_upload_needs_default_to_this_room_and_include_homeserver_sharing() {
        let mut store = PermissionStore::default();
        let lookup = FakeLookup { allowed: Default::default() };
        let plan = resolve(&request(vec![TaskNeed::Capability { id: "upload".into(),
            capability: "matrix.media.upload".into(), targets: vec![], why: None }]),
            &inputs(&store, &|_| true, &lookup)).unwrap();
        assert_eq!(plan.items[0].state, ItemState::NeedsGrant);
        assert_eq!(plan.items[0].action.scope(), Some(&RoomScope::room("!ai:example.org")));
        let homeserver = Recipient::network_origin("https://hs.example.org").unwrap();
        let sharing = plan.items.iter().find(|item| matches!(&item.action,
            PlanAction::Flow { source: Source::Room { room, .. }, recipient }
                if room == "!ai:example.org" && recipient == &homeserver)).unwrap();
        assert!(matches!(&sharing.origin, ItemOrigin::Implied { because } if because == &["upload"]));
        let approved: BTreeSet<String> = checked(&plan).into_iter().filter(|id| id != &sharing.id).collect();
        let applied = apply(&plan, &approved, &mut store, &FakeFlow::new()).unwrap();
        assert!(store.scoped_grants(&plan.subject).is_empty());
        assert_eq!(outcome_of(&plan, &approved, &applied)["not_granted"][0]["reason"], "declined_dependency");
    }

    #[test]
    fn media_upload_grants_cannot_be_scoped_to_another_rooms_agent() {
        let store = PermissionStore::default();
        let lookup = FakeLookup { allowed: Default::default() };
        for (target, expected) in [
            ("!ai:example.org", ItemState::NeedsGrant),
            ("!destination:example.org", ItemState::NotOffered(TaskReason::InvalidTarget)),
        ] {
            let plan = resolve(&request(vec![TaskNeed::Capability { id: "upload".into(),
                capability: "matrix.media.upload".into(), targets: vec![target.into()], why: None }]),
                &inputs(&store, &|_| true, &lookup)).unwrap();
            assert_eq!(plan.items[0].state, expected);
            assert_eq!(plan.items[0].action.scope(), Some(&RoomScope::room("!ai:example.org")));
        }
    }

    #[test]
    fn posting_in_this_room_depends_on_its_sharing_rule() {
        for capability in ["matrix.media.send"] {
            let mut store = PermissionStore::default();
            store.set_matrix_write(true);
            let lookup = FakeLookup { allowed: Default::default() };
            let plan = resolve(&request(vec![TaskNeed::Capability { id: "draft".into(),
                capability: capability.into(), targets: vec![], why: None }]),
                &inputs(&store, &|_| true, &lookup)).unwrap();
            assert_eq!(plan.items[0].state, ItemState::NeedsGrant);
            let sharing = plan.items.iter().find(|item| matches!(&item.action,
                PlanAction::Flow { source: Source::Room { room, .. }, recipient: Recipient::MatrixRoom { room: target, .. } }
                    if room == "!ai:example.org" && target == room)).unwrap();
            let approved: BTreeSet<String> = checked(&plan).into_iter().filter(|id| id != &sharing.id).collect();
            let applied = apply(&plan, &approved, &mut store, &FakeFlow::new()).unwrap();
            assert!(store.scoped_grants(&plan.subject).is_empty());
            assert_eq!(outcome_of(&plan, &approved, &applied)["not_granted"][0]["reason"], "declined_dependency");
        }
    }

    #[test]
    fn composer_only_needs_grant_drafts_without_sending_or_outgoing_sharing() {
        for capability in ["host.composer.insert", "host.composer.attach"] {
            let mut store = PermissionStore::default();
            let lookup = FakeLookup { allowed: Default::default() };
            store.set_room_policy("!target:example.org", RoomAccess::Write, PolicyDecision::Deny);
            store.set_room_spaces("!target:example.org", vec!["!parent:example.org".into()]);
            store.set_space_policy("!parent:example.org", RoomAccess::Write, PolicyDecision::Deny);
            for permission in [Permission::MatrixRoomSend, Permission::MatrixRoomsSend, Permission::MatrixMedia] {
                store.set("ai-room:!ai:example.org", permission, GrantState::Denied);
            }
            let plan = resolve(&request(vec![TaskNeed::Capability { id: "draft".into(),
                capability: capability.into(), targets: vec!["!target:example.org".into()], why: None }]),
                &inputs(&store, &|_| true, &lookup)).unwrap();
            assert_eq!(plan.items[0].state, ItemState::NeedsGrant);
            assert!(plan.items.iter().all(|item| !matches!(&item.action,
                PlanAction::Flow { recipient: Recipient::MatrixRoom { room, .. }, .. } if room == "!target:example.org")));
            // Baseline agent reply/provider rules remain separate from the
            // draft and do not need approval to prepare local content.
            assert!(plan.items.iter().all(|item| !matches!(&item.origin,
                ItemOrigin::Implied { because } if because.contains(&String::from("draft")))));
            let approved = [String::from("draft")].into_iter().collect();
            let applied = apply(&plan, &approved, &mut store, &FakeFlow::new()).unwrap();
            assert!(outcome_of(&plan, &approved, &applied)["granted"].as_array().unwrap().contains(&serde_json::json!("draft")));
            let grants = store.scoped_grants(&plan.subject);
            assert_eq!(grants.len(), 1);
            assert_eq!(grants[0].capability.as_deref(), Some(capability));
            assert!(!store.matrix_write());
            let context = PermissionContext { origin_room: Some("!ai:example.org"), target_room: Some("!target:example.org") };
            let cap = capabilities::by_id(capability).unwrap();
            assert_eq!(store.effective_capability_for_in_context(&plan.subject, |_| true, |_| true, cap, context), Effective::Granted);
            for send in ["matrix.room.message.send", "matrix.rooms.message.send", "matrix.media.send", "matrix.media.upload"] {
                assert_eq!(store.effective_capability_for_in_context(&plan.subject, |_| true, |_| true,
                    capabilities::by_id(send).unwrap(), context), Effective::Denied);
            }
        }
    }

    #[test]
    fn upfront_media_needs_respect_room_and_explicit_capability_denials() {
        for (capability, access) in [("matrix.media.send", RoomAccess::Write), ("host.composer.attach", RoomAccess::Read)] {
            let mut store = PermissionStore::default();
            store.set_matrix_write(true);
            let lookup = FakeLookup { allowed: Default::default() };
            let need = request(vec![TaskNeed::Capability { id: "media".into(),
                capability: capability.into(), targets: vec!["!target:example.org".into()], why: None }]);
            let plan = resolve(&need, &inputs(&store, &|_| true, &lookup)).unwrap();
            assert_eq!(plan.items[0].state, ItemState::NeedsGrant);
            store.set_room_policy("!target:example.org", access, PolicyDecision::Deny);
            let plan = resolve(&need, &inputs(&store, &|_| true, &lookup)).unwrap();
            assert_eq!(plan.items[0].state, ItemState::Blocked(TaskReason::BlockedByRoomPolicy));
            store.set_room_policy("!target:example.org", access, PolicyDecision::Ask);
            store.set_capability("ai-room:!ai:example.org", capability, GrantState::Denied);
            let plan = resolve(&need, &inputs(&store, &|_| true, &lookup)).unwrap();
            assert_eq!(plan.items[0].state, ItemState::Blocked(TaskReason::BlockedByPermission));
        }
        let mut store = PermissionStore::default();
        store.set("ai-room:!ai:example.org", Permission::MatrixMedia, GrantState::Denied);
        let lookup = FakeLookup { allowed: Default::default() };
        let plan = resolve(&request(vec![TaskNeed::Capability { id: "upload".into(),
            capability: "matrix.media.upload".into(), targets: vec![], why: None }]),
            &inputs(&store, &|_| true, &lookup)).unwrap();
        assert_eq!(plan.items[0].state, ItemState::Blocked(TaskReason::BlockedByPermission));
    }

    #[test]
    fn matrix_attachment_preparation_uses_source_room_reads_and_homeserver_sharing() {
        let mut store = PermissionStore::default();
        let lookup = FakeLookup { allowed: Default::default() };
        let need = request(vec![TaskNeed::Capability { id: "fetch".into(),
            capability: "matrix.media.download".into(), targets: vec!["!source:example.org".into()], why: None }]);
        let plan = resolve(&need, &inputs(&store, &|_| true, &lookup)).unwrap();
        assert_eq!(plan.items[0].state, ItemState::NeedsGrant, "downloading does not need the room write switch");
        let source = Source::Room { account: "alice".into(), room: "!source:example.org".into() };
        for recipient in [Recipient::ModelProvider("provider".into()),
            Recipient::network_origin("https://hs.example.org").unwrap()]
        {
            assert!(plan.items.iter().any(|item| matches!(&item.action,
                PlanAction::Flow { source: item_source, recipient: item_recipient }
                    if item_source == &source && item_recipient == &recipient)));
        }
        store.set_room_policy("!source:example.org", RoomAccess::Write, PolicyDecision::Deny);
        let plan = resolve(&need, &inputs(&store, &|_| true, &lookup)).unwrap();
        assert_eq!(plan.items[0].state, ItemState::NeedsGrant);
        store.set_room_policy("!source:example.org", RoomAccess::Read, PolicyDecision::Deny);
        let plan = resolve(&need, &inputs(&store, &|_| true, &lookup)).unwrap();
        assert_eq!(plan.items[0].state, ItemState::Blocked(TaskReason::BlockedByRoomPolicy));
    }

    #[test]
    fn requested_ids_cannot_collide_with_host_sharing_rows() {
        let collision = "flow:{\"Room\":{\"account\":\"alice\",\"room\":\"!ai:example.org\"}}:{\"NetworkOrigin\":\"https://status.example.com\"}";
        for id in [collision, "  flow:reserved"] {
            assert!(parse_request(&serde_json::json!({"task": "Fetch", "needs": [
                {"kind": "website", "id": id, "url": "https://status.example.com/page"}
            ]})).unwrap_err().contains("reserved"));
        }
        // Defense in depth: even a host-built plan cannot alias two checkbox
        // rows and then grant the unchecked one under the selected row's id.
        let store = PermissionStore::default();
        let lookup = FakeLookup { allowed: Default::default() };
        let mut plan = resolve(&request(vec![website()]), &inputs(&store, &|_| true, &lookup)).unwrap();
        let sharing = plan.items.iter().find(|item| matches!(item.action, PlanAction::Flow { .. })).unwrap().id.clone();
        plan.items[0].id = sharing;
        plan.seal();
        assert!(apply(&plan, &checked(&plan).into_iter().collect(), &mut PermissionStore::default(), &FakeFlow::new()).is_err());
    }

    #[test]
    fn an_existing_read_allowance_can_acquire_missing_sharing_rules() {
        let mut store = PermissionStore::default();
        store.grant_scoped("ai-room:!ai:example.org", Permission::MatrixRoomsRead, Some("matrix.rooms.messages.read"),
            RoomScope::room("!ops:example.org"), GrantDuration::Always, None).unwrap();
        let lookup = FakeLookup { allowed: Default::default() };
        let plan = resolve(&request(vec![read("read", "!ops:example.org")]), &inputs(&store, &|_| true, &lookup)).unwrap();
        assert_eq!(plan.items[0].state, ItemState::AlreadyAllowed);
        let approved = dependent_approval(&plan, checked(&plan));
        assert!(!approved.contains("read"), "the UI emits only enabled checkbox ids");
        let flow = FakeFlow::new();
        let applied = apply(&plan, &approved, &mut store, &flow).unwrap();
        let source = Source::Room { account: "alice".into(), room: "!ops:example.org".into() };
        assert!(flow.allowed.borrow().contains(&(item_key(&source), item_key(&Recipient::ModelProvider("provider".into())))));
        assert_eq!(store.scoped_grants(&plan.subject).len(), 1, "the existing permission is not duplicated");
        assert_eq!(outcome_of(&plan, &approved, &applied)["status"], "granted");
    }

    #[test]
    fn selecting_one_of_two_reads_keeps_their_shared_flow() {
        let mut store = PermissionStore::default();
        let lookup = FakeLookup { allowed: Default::default() };
        let plan = resolve(&request(vec![read("omit", "!ops:example.org"), read("keep", "!ops:example.org"), website()]),
            &inputs(&store, &|_| true, &lookup)).unwrap();
        let approved = dependent_approval(&plan, checked(&plan).into_iter().filter(|id| id != "omit"));
        let flow = FakeFlow::new();
        let applied = apply(&plan, &approved, &mut store, &flow).unwrap();
        assert_eq!(store.scoped_grants(&plan.subject).len(), 1);
        assert_eq!(store.network_grants(&plan.subject).len(), 1);
        assert_eq!(outcome_of(&plan, &approved, &applied)["granted"], serde_json::json!(["keep", "website"]));
    }

    #[test]
    fn an_unselected_read_does_not_disable_a_selected_website() {
        let mut store = PermissionStore::default();
        let lookup = FakeLookup { allowed: Default::default() };
        let plan = resolve(&request(vec![read("omit", "!private:example.org"), read("keep", "!ops:example.org"), website()]),
            &inputs(&store, &|_| true, &lookup)).unwrap();
        let approved = dependent_approval(&plan, checked(&plan).into_iter().filter(|id| id != "omit"));
        let flow = FakeFlow::new();
        let applied = apply(&plan, &approved, &mut store, &flow).unwrap();
        let source = Source::Room { account: "alice".into(), room: "!private:example.org".into() };
        assert!(!flow.allowed.borrow().iter().any(|(s, _)| s == &item_key(&source)), "an omitted read creates no sharing grants");
        assert_eq!(store.network_grants(&plan.subject).len(), 1);
        assert_eq!(outcome_of(&plan, &approved, &applied)["granted"], serde_json::json!(["keep", "website"]));
    }

    #[test]
    fn an_unchecked_provider_rule_drops_its_read_without_dropping_other_outputs() {
        let mut store = PermissionStore::default();
        let lookup = FakeLookup { allowed: Default::default() };
        let plan = resolve(&request(vec![read("read", "!ops:example.org"), website()]), &inputs(&store, &|_| true, &lookup)).unwrap();
        let unchecked = plan.items.iter().find(|item| matches!(&item.action,
            PlanAction::Flow { source: Source::Room { room, .. }, recipient: Recipient::ModelProvider(_) } if room == "!ops:example.org")).unwrap().id.clone();
        let approved = dependent_approval(&plan, checked(&plan).into_iter().filter(|id| id != &unchecked));
        let flow = FakeFlow::new();
        let applied = apply(&plan, &approved, &mut store, &flow).unwrap();
        assert!(store.scoped_grants(&plan.subject).is_empty());
        assert_eq!(store.network_grants(&plan.subject).len(), 1);
        let result = outcome_of(&plan, &approved, &applied);
        assert_eq!(result["granted"], serde_json::json!(["website"]));
        assert_eq!(result["not_granted"][0]["reason"], "declined_dependency");
    }

    #[test]
    fn an_unchecked_output_rule_never_releases_a_source_already_in_memory() {
        let mut store = PermissionStore::default();
        let source = Source::Room { account: "alice".into(), room: "!ops:example.org".into() };
        let lookup = GrownLookup { grown: vec![source.clone()] };
        let plan = resolve(&request(vec![read("omit", "!ops:example.org"), website()]), &inputs(&store, &|_| true, &lookup)).unwrap();
        let unchecked = plan.items.iter().find(|item| matches!(&item.action,
            PlanAction::Flow { source: s, recipient: Recipient::NetworkOrigin(origin) } if s == &source && origin == "https://status.example.com")).unwrap().id.clone();
        let approved = dependent_approval(&plan, checked(&plan).into_iter().filter(|id| id != "omit" && id != &unchecked));
        let flow = FakeFlow::new();
        let applied = apply(&plan, &approved, &mut store, &flow).unwrap();
        assert!(store.network_grants(&plan.subject).is_empty(), "existing knowledge still needs sharing consent");
        assert!(!flow.allowed.borrow().contains(&(item_key(&source), item_key(&Recipient::network_origin("https://status.example.com").unwrap()))));
        assert!(outcome_of(&plan, &approved, &applied)["granted"].as_array().unwrap().is_empty());
    }

    #[test]
    fn app_tool_grants_use_the_canonical_name_and_existing_grants_need_no_regrant() {
        let mut store = PermissionStore::default();
        let lookup = FakeLookup { allowed: Default::default() };
        let joined = |_: &str| true;
        let resolve_name = |tool: &str| match tool {
            "ttt_play" | "app_tic-tac-toe_ttt_play" => Some("app_tic-tac-toe_ttt_play".to_string()),
            _ => None,
        };
        let mut resolve_inputs = inputs(&store, &joined, &lookup);
        resolve_inputs.app_tool_name = &resolve_name;
        let need = || TaskNeed::AppTool { id: "tool".into(), tool: "ttt_play".into(), why: None };
        let plan = resolve(&request(vec![need()]), &resolve_inputs).unwrap();
        assert!(matches!(&plan.items[0].action, PlanAction::Tool { tool, .. } if tool == "app_tic-tac-toe_ttt_play"));
        let approved = dependent_approval(&plan, checked(&plan));
        apply(&plan, &approved, &mut store, &FakeFlow::new()).unwrap();
        assert!(store.has_scoped_tool_grant(&plan.subject, "app_tic-tac-toe_ttt_play", "",
            PermissionContext { origin_room: Some("!ai:example.org"), target_room: Some("!ai:example.org") }));
        let mut resolve_inputs = inputs(&store, &joined, &lookup);
        resolve_inputs.app_tool_name = &resolve_name;
        let second = resolve(&request(vec![need()]), &resolve_inputs).unwrap();
        assert_eq!(second.items[0].state, ItemState::AlreadyAllowed);
        let missing = resolve(&request(vec![TaskNeed::AppTool { id: "missing".into(), tool: "ambiguous-or-foreign".into(), why: None }]), &resolve_inputs).unwrap();
        assert_eq!(missing.items[0].state, ItemState::NotOffered(TaskReason::InvalidTarget));
    }

    #[test]
    fn app_catalog_needs_default_to_the_requesting_room() {
        let mut store = PermissionStore::default();
        let lookup = FakeLookup { allowed: Default::default() };
        let plan = resolve(&request(vec!["apps.list", "apps.launch"].into_iter().enumerate().map(|(n, cap)| TaskNeed::Capability {
            id: format!("n{n}"), capability: cap.into(), targets: Vec::new(), why: None,
        }).collect()), &inputs(&store, &|_| true, &lookup)).unwrap();
        assert!(plan.items[..2].iter().all(|item| item.state.is_grantable() && item.action.scope() == Some(&RoomScope::room("!ai:example.org"))));
        let approved = dependent_approval(&plan, checked(&plan));
        let applied = apply(&plan, &approved, &mut store, &FakeFlow::new()).unwrap();
        assert_eq!(outcome_of(&plan, &approved, &applied)["status"], "granted");
    }

    #[test]
    fn explicit_denials_are_blocked_at_resolution_and_at_apply_time() {
        let denials: [fn(&mut PermissionStore, &str); 7] = [
            |store, subject| store.set(subject, Permission::MatrixRoomsRead, GrantState::Denied),
            |store, subject| store.set_capability(subject, "matrix.rooms.messages.read", GrantState::Denied),
            |store, subject| store.set(subject, Permission::Network, GrantState::Denied),
            |store, subject| store.set_capability(subject, "network.http", GrantState::Denied),
            |store, subject| store.set(subject, Permission::McpTools, GrantState::Denied),
            |store, subject| store.deny_tool(subject, "tool"),
            |store, subject| store.restrict(subject, "User restricted this agent", 0, 0),
        ];
        let needs = || vec![read("read", "!ops:example.org"), website(),
            TaskNeed::AppTool { id: "tool".into(), tool: "tool".into(), why: None }];
        let lookup = FakeLookup { allowed: Default::default() };
        for (case, deny) in denials.into_iter().enumerate() {
            let index = if case < 2 { 0 } else if case < 4 { 1 } else { 2 };
            let mut store = PermissionStore::default();
            deny(&mut store, "ai-room:!ai:example.org");
            let denied = resolve(&request(needs()), &inputs(&store, &|_| true, &lookup)).unwrap();
            assert_eq!(denied.items[index].state, ItemState::Blocked(TaskReason::BlockedByPermission), "case {case}");
            let mut store = PermissionStore::default();
            let plan = resolve(&request(needs()), &inputs(&store, &|_| true, &lookup)).unwrap();
            let approved = dependent_approval(&plan, checked(&plan));
            deny(&mut store, &plan.subject);
            let applied = apply(&plan, &approved, &mut store, &FakeFlow::new()).unwrap();
            assert_eq!(applied.item_states.get(&plan.items[index].id), Some(&ItemState::Blocked(TaskReason::BlockedByPermission)));
            assert!(!outcome_of(&plan, &approved, &applied)["granted"].as_array().unwrap().contains(&serde_json::json!(plan.items[index].id)));
        }
    }

    #[test]
    fn existing_allowances_denied_while_the_modal_waits_are_not_reported_granted() {
        let mut store = PermissionStore::default();
        let subject = "ai-room:!ai:example.org";
        store.grant_scoped(subject, Permission::MatrixRoomsRead, Some("matrix.rooms.messages.read"),
            RoomScope::room("!ops:example.org"), GrantDuration::Always, None).unwrap();
        store.allow_network(subject, NetworkScope::ExactUrl("https://status.example.com/page".into()),
            RoomScope::room("!ai:example.org"), GrantDuration::Always, None).unwrap();
        store.grant_scoped_tool(subject, "tool", "", RoomScope::room("!ai:example.org"), GrantDuration::Always, None).unwrap();
        let lookup = FakeLookup { allowed: Default::default() };
        let plan = resolve(&request(vec![read("read", "!ops:example.org"), website(),
            TaskNeed::AppTool { id: "tool".into(), tool: "tool".into(), why: None }]), &inputs(&store, &|_| true, &lookup)).unwrap();
        assert!(plan.items[..3].iter().all(|item| item.state == ItemState::AlreadyAllowed));
        let approved = dependent_approval(&plan, checked(&plan));
        store.set(subject, Permission::MatrixRoomsRead, GrantState::Denied);
        store.set(subject, Permission::Network, GrantState::Denied);
        store.set(subject, Permission::McpTools, GrantState::Denied);
        let flow = FakeFlow::new();
        let applied = apply(&plan, &approved, &mut store, &flow).unwrap();
        let result = outcome_of(&plan, &approved, &applied);
        assert_eq!(result["status"], "blocked");
        assert!(result["granted"].as_array().unwrap().is_empty());
        assert!(!flow.allowed.borrow().iter().any(|(source, _)| source.contains("!ops:example.org")));
    }

    #[test]
    fn an_existing_read_revoked_while_the_modal_waits_is_not_silently_regranted() {
        let mut store = PermissionStore::default();
        let grant = store.grant_scoped("ai-room:!ai:example.org", Permission::MatrixRoomsRead, Some("matrix.rooms.messages.read"),
            RoomScope::room("!ops:example.org"), GrantDuration::Always, None).unwrap();
        let lookup = FakeLookup { allowed: Default::default() };
        let plan = resolve(&request(vec![read("read", "!ops:example.org")]), &inputs(&store, &|_| true, &lookup)).unwrap();
        let approved = dependent_approval(&plan, checked(&plan));
        store.remove_scoped_grant(grant);
        let applied = apply(&plan, &approved, &mut store, &FakeFlow::new()).unwrap();
        assert!(store.scoped_grants(&plan.subject).is_empty());
        assert_eq!(outcome_of(&plan, &approved, &applied)["status"], "declined");
    }

    #[test]
    fn reading_a_room_does_not_grant_output_into_it_but_sending_still_requires_output() {
        let mut store = PermissionStore::default();
        let lookup = FakeLookup { allowed: Default::default() };
        let target = Recipient::MatrixRoom { account: "alice".into(), room: "!ops:example.org".into() };
        let read_plan = resolve(&request(vec![read("read", "!ops:example.org")]),
            &inputs(&store, &|_| true, &lookup)).unwrap();
        assert!(!read_plan.items.iter().any(|item| matches!(&item.action,
            PlanAction::Flow { recipient, .. } if recipient == &target)), "a read does not release data into the read room");
        let approved = dependent_approval(&read_plan, checked(&read_plan));
        let flow = FakeFlow::new();
        let applied = apply(&read_plan, &approved, &mut store, &flow).unwrap();
        assert_eq!(outcome_of(&read_plan, &approved, &applied)["granted"], serde_json::json!(["read"]));
        let read_source = Source::Room { account: "alice".into(), room: "!ops:example.org".into() };
        for recipient in [Recipient::ModelProvider("provider".into()),
            Recipient::network_origin("https://hs.example.org").unwrap(),
            Recipient::MatrixRoom { account: "alice".into(), room: "!ai:example.org".into() }]
        {
            assert!(flow.allowed.borrow().contains(&(item_key(&read_source), item_key(&recipient))));
        }
        assert!(!flow.allowed.borrow().iter().any(|(_, recipient)| recipient == &item_key(&target)));

        let mut store = PermissionStore::default();
        store.set_matrix_write(true);
        let send_plan = resolve(&request(vec![TaskNeed::Capability { id: "send".into(),
            capability: "matrix.rooms.message.send".into(), targets: vec!["!ops:example.org".into()], why: None }]),
            &inputs(&store, &|_| true, &lookup)).unwrap();
        let room_output = send_plan.items.iter().find(|item| matches!(&item.action,
            PlanAction::Flow { recipient, .. } if recipient == &target)).expect("a send requires its room recipient");
        let narrowed = dependent_approval(&send_plan, checked(&send_plan).into_iter().filter(|id| id != &room_output.id));
        let applied = apply(&send_plan, &narrowed, &mut store, &FakeFlow::new()).unwrap();
        assert!(store.scoped_grants(&send_plan.subject).is_empty(), "an unchecked room output prevents the send grant");
        assert_eq!(outcome_of(&send_plan, &narrowed, &applied)["not_granted"][0]["reason"], "declined_dependency");

        let approved = dependent_approval(&send_plan, checked(&send_plan));
        let flow = FakeFlow::new();
        let applied = apply(&send_plan, &approved, &mut store, &flow).unwrap();
        assert_eq!(outcome_of(&send_plan, &approved, &applied)["granted"], serde_json::json!(["send"]));
        assert!(flow.allowed.borrow().iter().any(|(_, recipient)| recipient == &item_key(&target)));
    }

    #[test]
    fn space_directory_needs_do_not_require_exporting_into_each_space() {
        let store = PermissionStore::default();
        let lookup = FakeLookup { allowed: Default::default() };
        for cap in ["matrix.space.info.read", "matrix.space.rooms.list"] {
            let plan = resolve(&request(vec![TaskNeed::Capability { id: "directory".into(), capability: cap.into(),
                targets: vec!["!space:example.org".into()], why: None }]), &inputs(&store, &|_| true, &lookup)).unwrap();
            assert!(plan.items.iter().any(|item| matches!(item.action,
                PlanAction::Flow { source: Source::RoomDirectory { .. }, recipient: Recipient::ModelProvider(_) })));
            assert!(!plan.items.iter().any(|item| matches!(&item.action,
                PlanAction::Flow { source: Source::Room { room, .. }, .. }
                    | PlanAction::Flow { recipient: Recipient::MatrixRoom { room, .. }, .. } if room == "!space:example.org")));
        }
    }

    #[test]
    fn a_revoked_sharing_allowance_is_not_replaced_without_a_checked_row() {
        let mut store = PermissionStore::default();
        let source = Source::Room { account: "alice".into(), room: "!ops:example.org".into() };
        let provider = Recipient::ModelProvider("provider".into());
        let lookup = FakeLookup { allowed: [(item_key(&source), item_key(&provider))].into_iter().collect() };
        let plan = resolve(&request(vec![read("read", "!ops:example.org")]), &inputs(&store, &|_| true, &lookup)).unwrap();
        let approved = dependent_approval(&plan, checked(&plan));
        let flow = FakeFlow::new(); // The provider rule was revoked after resolution.
        let applied = apply(&plan, &approved, &mut store, &flow).unwrap();
        assert!(store.scoped_grants(&plan.subject).is_empty());
        assert!(!flow.allowed.borrow().contains(&(item_key(&source), item_key(&provider))));
        assert_eq!(outcome_of(&plan, &approved, &applied)["not_granted"][0]["reason"], "declined_dependency");
    }

    #[test]
    fn sharing_granted_while_the_modal_waits_is_not_owned_by_the_task() {
        let mut store = PermissionStore::default();
        let lookup = FakeLookup { allowed: Default::default() };
        let plan = resolve(&request(vec![read("read", "!ops:example.org")]), &inputs(&store, &|_| true, &lookup)).unwrap();
        let approved = dependent_approval(&plan, checked(&plan));
        let source = Source::Room { account: "alice".into(), room: "!ops:example.org".into() };
        let provider = Recipient::ModelProvider("provider".into());
        let flow = FakeFlow::new();
        flow.allowed.borrow_mut().insert((item_key(&source), item_key(&provider)));
        let applied = apply(&plan, &approved, &mut store, &flow).unwrap();
        let inherited = plan.items.iter().find(|item| matches!(&item.action,
            PlanAction::Flow { source: s, recipient: r } if s == &source && r == &provider)).unwrap();
        assert_eq!(applied.item_states.get(&inherited.id), Some(&ItemState::AlreadyAllowed));
        let created = applied.grants.iter().filter(|grant| matches!(grant, GrantRef::Flow(_))).count();
        assert_eq!(created + 1, plan.grantable().filter(|item| matches!(item.action, PlanAction::Flow { .. })).count());
        assert_eq!(outcome_of(&plan, &approved, &applied)["status"], "granted");
    }

    #[test]
    fn a_stale_context_is_refused_before_a_permission_only_selection_is_applied() {
        let mut store = PermissionStore::default();
        let lookup = FakeLookup { allowed: Default::default() };
        let plan = resolve(&request(vec![TaskNeed::AppTool { id: "tool".into(), tool: "tool".into(), why: None }]),
            &inputs(&store, &|_| true, &lookup)).unwrap();
        let flow = FakeFlow::new();
        flow.stale.set(true);
        assert_eq!(apply(&plan, &["tool".to_string()].into_iter().collect(), &mut store, &flow), Err(ApplyError::StaleContext));
        assert!(store.scoped_grants(&plan.subject).is_empty());
        assert!(flow.allowed.borrow().is_empty());
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
    fn needs_fingerprint_ignores_agent_labels_and_new_needs_differ() {
        let mut store = PermissionStore::default();
        store.set_matrix_write(true);
        let joined = |room: &str| matches!(room, "!ai:example.org" | "!ops:example.org" | "!leads:example.org");
        let lookup = FakeLookup { allowed: Default::default() };
        let send = |id: &str| TaskNeed::Capability { id: id.into(), capability: "matrix.rooms.message.send".into(),
            targets: vec!["!leads:example.org".into()], why: Some("post the digest".into()) };
        let first = resolve(&TaskRequest { task: "Digest".into(), explanation: "first wording".into(), needs: vec![send("n1")] },
            &inputs(&store, &joined, &lookup)).unwrap();
        // A rename of the need id and of the prose must not defeat "Not now".
        let renamed = resolve(&TaskRequest { task: "Another title".into(), explanation: "different wording".into(), needs: vec![send("z9")] },
            &inputs(&store, &joined, &lookup)).unwrap();
        assert_eq!(first.needs_fingerprint, renamed.needs_fingerprint);
        assert!(renamed.need_keys().is_subset(&first.need_keys()));
        // A request containing something new is a strict superset and is not
        // covered by the earlier dismissal.
        let more = resolve(&TaskRequest { task: "Digest plus".into(), explanation: "more".into(), needs: vec![send("a"),
            TaskNeed::Capability { id: "b".into(), capability: "matrix.rooms.messages.read".into(),
                targets: vec!["!ops:example.org".into()], why: None }] }, &inputs(&store, &joined, &lookup)).unwrap();
        assert!(!more.need_keys().is_subset(&first.need_keys()));
        assert!(first.need_keys().is_subset(&more.need_keys()));
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

    /// The store can change while the modal waits. A room the user denies in
    /// the meantime is reported blocked at apply time and nothing is granted
    /// for it; the rest of the batch still applies.
    #[test]
    fn apply_rechecks_room_policy_and_reports_a_new_denial() {
        let mut store = PermissionStore::default();
        store.set_matrix_write(true);
        let joined = |room: &str| matches!(room, "!ai:example.org" | "!leads:example.org");
        let lookup = FakeLookup { allowed: Default::default() };
        let plan = resolve(&request(vec![TaskNeed::Capability { id: "n1".into(),
            capability: "matrix.rooms.message.send".into(), targets: vec!["!leads:example.org".into()], why: None }]),
            &inputs(&store, &joined, &lookup)).unwrap();
        let approved: BTreeSet<String> = plan.items.iter().map(|i| i.id.clone()).collect();
        // The user denies the target room while the prompt waits.
        store.set_room_policy("!leads:example.org", RoomAccess::Write, PolicyDecision::Deny);
        let flow = FakeFlow::new();
        let applied = apply(&plan, &approved, &mut store, &flow).unwrap();
        assert_eq!(applied.item_states.get("n1"), Some(&ItemState::Blocked(TaskReason::BlockedByRoomPolicy)));
        assert!(store.scoped_grants(&plan.subject).is_empty(), "a denied item is not granted");
        let outcome = outcome_of(&plan, &approved, &applied);
        assert_eq!(outcome["not_granted"][0]["reason"], "blocked_by_room_policy");
        // A blocked read must not leave its own implied flow rules granted.
        let leads = Source::Room { account: "alice".into(), room: "!leads:example.org".into() };
        assert!(
            !flow.allowed.borrow().iter().any(|(source, _)| *source == item_key(&leads)),
            "a blocked read's flow rules must be skipped with it"
        );
        for item in plan.items.iter() {
            if let ItemOrigin::Implied { because } = &item.origin
                && because.contains(&"n1".to_string())
            {
                assert_eq!(applied.item_states.get(&item.id), Some(&ItemState::Blocked(TaskReason::BlockedByRoomPolicy)),
                    "{} must be reported blocked", item.id);
            }
        }
    }

    /// A read whose implied sharing rule the user left unchecked is not
    /// granted: the capability would sit in the ledger while its data could
    /// never reach the agent. The outcome reports it `declined_dependency`.
    #[test]
    fn apply_drops_a_read_whose_flow_dependency_was_unchecked() {
        let mut store = PermissionStore::default();
        let joined = |room: &str| matches!(room, "!ai:example.org" | "!ops:example.org");
        let lookup = FakeLookup { allowed: Default::default() };
        let plan = resolve(&request(vec![TaskNeed::Capability { id: "n1".into(),
            capability: "matrix.rooms.messages.read".into(), targets: vec!["!ops:example.org".into()], why: None }]),
            &inputs(&store, &joined, &lookup)).unwrap();
        // The user approves the read but unchecks the provider sharing rule.
        let flow_item = plan.items.iter().find(|item| {
            matches!(&item.action, PlanAction::Flow { recipient: Recipient::ModelProvider(_), .. })
                && matches!(&item.origin, ItemOrigin::Implied { because } if because.contains(&"n1".to_string()))
        }).expect("the read derives a provider flow row").id.clone();
        let approved: BTreeSet<String> = plan.items.iter().map(|item| item.id.clone())
            .filter(|id| id != &flow_item).collect();
        let flow = FakeFlow::new();
        let applied = apply(&plan, &approved, &mut store, &flow).unwrap();
        assert!(store.scoped_grants(&plan.subject).is_empty(),
            "a read without its sharing rule must not record its capability grant");
        let outcome = outcome_of(&plan, &approved, &applied);
        assert!(outcome["not_granted"].as_array().unwrap().iter().any(|entry| entry["reason"] == "declined_dependency"));
        assert_eq!(outcome["status"], "partial");
    }

    /// Granting the capability between resolve and apply must not produce a
    /// second, duplicate grant.
    #[test]
    fn apply_does_not_duplicate_a_capability_granted_while_the_modal_waited() {        let mut store = PermissionStore::default();
        store.set_matrix_write(true);
        let joined = |room: &str| matches!(room, "!ai:example.org" | "!leads:example.org");
        let lookup = FakeLookup { allowed: Default::default() };
        let plan = resolve(&request(vec![TaskNeed::Capability { id: "n1".into(),
            capability: "matrix.rooms.message.send".into(), targets: vec!["!leads:example.org".into()], why: None }]),
            &inputs(&store, &joined, &lookup)).unwrap();
        let approved: BTreeSet<String> = plan.items.iter().map(|i| i.id.clone()).collect();
        // Something else grants it while the prompt waits.
        store.grant_scoped(&plan.subject, Permission::MatrixRoomsSend, Some("matrix.rooms.message.send"),
            RoomScope::room("!leads:example.org"), GrantDuration::RobrixSession, Some("!ai:example.org")).unwrap();
        let before = store.scoped_grants(&plan.subject).len();
        let flow = FakeFlow::new();
        let applied = apply(&plan, &approved, &mut store, &flow).unwrap();
        assert_eq!(applied.item_states.get("n1"), Some(&ItemState::AlreadyAllowed));
        assert_eq!(store.scoped_grants(&plan.subject).len(), before, "no duplicate grant is added");
        assert_eq!(outcome_of(&plan, &approved, &applied)["granted"][0], "n1");
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
            epoch: 1, title: "t".into(), explanation: "e".into(), items: Vec::new(), flow_dependencies: BTreeMap::new(), plan_hash: [0; 32], needs_fingerprint: [0; 32] };
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
    fn cleaning_strips_direction_controls_from_agent_text() {
        // An RLO could visually reverse the rest of the paragraph.
        assert_eq!(clean_text("safe\u{202e}evil", 100), "safeevil");
        // Bidi isolates and directional marks are removed too.
        assert_eq!(clean_text("\u{2066}a\u{2069}\u{200f}b", 100), "ab");
        // The title, which reaches the tool-call note, is sanitized as well.
        assert_eq!(clip("\u{202e}Important", 100), "Important");
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
    fn outcome_status_counts_already_allowed_and_separates_blocked_from_declined() {
        let make = |states: Vec<ItemState>| {
            let context = ContextId::Agent { account: "a".into(), room: "r".into() };
            let items = states.into_iter().enumerate().map(|(index, state)| PlanItem {
                id: format!("n{index}"), origin: ItemOrigin::Requested, risk: Risk::Low,
                action: PlanAction::Network { url: "https://example.com/".into(), scope: RoomScope::room("r") },
                state, why: None,
            }).collect();
            TaskPlan { task_id: 1, subject: "s".into(), context, epoch: 1, title: "t".into(),
                explanation: "e".into(), plan_hash: [0; 32], needs_fingerprint: [0; 32], items, flow_dependencies: BTreeMap::new() }
        };
        let empty = BTreeSet::new();
        // Everything already allowed is granted, not declined.
        let value = outcome(&make(vec![ItemState::AlreadyAllowed]), &empty);
        assert_eq!(value["status"], "granted");
        // Everything blocked is blocked, not declined.
        let value = outcome(&make(vec![ItemState::Blocked(TaskReason::BlockedByRoomPolicy)]), &empty);
        assert_eq!(value["status"], "blocked");
        // A requested need the user said no to is declined.
        let value = outcome(&make(vec![ItemState::NeedsGrant]), &empty);
        assert_eq!(value["status"], "declined");
        // A mix is partial.
        let value = outcome(&make(vec![ItemState::AlreadyAllowed, ItemState::Blocked(TaskReason::NotOffered)]), &empty);
        assert_eq!(value["status"], "partial");
    }

    #[test]
    fn outcome_keeps_implied_flow_ids_internal_and_reports_a_declined_dependency() {
        let mut plan = TaskPlan {
            task_id: 1, subject: "s".into(), context: ContextId::Agent { account: "a".into(), room: "r".into() },
            epoch: 1, title: "t".into(), explanation: "e".into(), plan_hash: [0; 32], needs_fingerprint: [0; 32], flow_dependencies: BTreeMap::new(),
            items: vec![
                PlanItem { id: "n1".into(), origin: ItemOrigin::Requested, risk: Risk::High, why: None,
                    action: PlanAction::Network { url: "https://example.com/".into(), scope: RoomScope::room("r") }, state: ItemState::NeedsGrant },
                PlanItem { id: "flow:1".into(), origin: ItemOrigin::Implied { because: vec!["n1".into()] }, risk: Risk::High, why: None,
                    action: PlanAction::Flow { source: Source::Room { account: "a".into(), room: "r".into() },
                        recipient: Recipient::network_origin("https://example.com/").unwrap() }, state: ItemState::NeedsGrant },
            ],
        };
        plan.seal();
        // The read is approved but its implied flow row is left unchecked.
        let approved: BTreeSet<String> = ["n1".to_string()].into_iter().collect();
        let value = outcome(&plan, &approved);
        assert_eq!(value["flow_rules"]["applied"], 0);
        assert_eq!(value["flow_rules"]["skipped"], 1);
        let granted: Vec<&str> = value["granted"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
        assert!(!granted.contains(&"n1"), "a read without its dependency is not a clean grant");
        assert!(!granted.iter().any(|id| id.starts_with("flow:")), "flow ids stay internal");
        let reasons: Vec<&str> = value["not_granted"].as_array().unwrap().iter().filter_map(|e| e["reason"].as_str()).collect();
        assert!(reasons.contains(&"declined_dependency"));
        assert_eq!(value["status"], "partial");
        // With the dependency approved, the read is a clean grant and the flow
        // row is counted, never listed.
        let approved: BTreeSet<String> = ["n1".to_string(), "flow:1".to_string()].into_iter().collect();
        let value = outcome(&plan, &approved);
        assert_eq!(value["flow_rules"]["applied"], 1);
        assert_eq!(value["flow_rules"]["skipped"], 0);
        assert_eq!(value["status"], "granted");
        assert_eq!(value["granted"], serde_json::json!(["n1"]), "only the requested need is listed");
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
    fn a_failed_apply_rolls_back_already_granted_permissions() {
        let mut store = PermissionStore::default();
        store.set_matrix_write(true);
        let joined = |room: &str| matches!(room, "!ai:example.org" | "!ops:example.org");
        let lookup = FakeLookup { allowed: Default::default() };
        let plan = resolve(&request(vec![TaskNeed::Capability { id: "n1".into(),
            capability: "matrix.rooms.messages.read".into(), targets: vec!["!ops:example.org".into()], why: None }]),
            &inputs(&store, &joined, &lookup)).unwrap();
        let approved: BTreeSet<String> = plan.items.iter().map(|item| item.id.clone()).collect();
        let flow = FakeFlow::new();
        // The information-flow grant is the last thing applied; failing it must
        // roll back the permission grant already made for `n1`.
        flow.fail_next.set(true);
        let result = apply(&plan, &approved, &mut store, &flow);
        assert!(matches!(result, Err(ApplyError::Failed { .. })));
        assert!(store.scoped_grants(&plan.subject).is_empty(), "a partial apply must leave no permission grant");
    }

    #[test]
    fn unchecking_a_read_drops_the_flow_rules_that_depend_on_it() {
        let plan = TaskPlan {
            task_id: 1, subject: "s".into(), context: ContextId::Agent { account: "a".into(), room: "r".into() },
            epoch: 1, title: "t".into(), explanation: "e".into(), plan_hash: [0; 32], needs_fingerprint: [0; 32], flow_dependencies: BTreeMap::new(),
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
            epoch: 1, title: "t".into(), plan_hash: [0; 32], grants: vec![GrantRef::Scoped(2)], item_states: BTreeMap::new() });
        assert_eq!(ledger.tasks().count(), 1);
        assert!(ledger.remove(3).is_some());
        assert!(ledger.is_empty());
        ledger.insert(AppliedTask { task_id: 4, subject: "s".into(), context: ContextId::Agent { account: "a".into(), room: "r".into() },
            epoch: 1, title: "t".into(), plan_hash: [0; 32], grants: vec![], item_states: BTreeMap::new() });
        assert_eq!(ledger.drain().len(), 1);
    }
}
