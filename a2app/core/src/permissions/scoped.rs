//! Room and space boundaries are host policy, above individual app grants.
//! Session lifetimes describe the host session, never an isolate restart.

use super::*;
use crate::capabilities::{Access, Capability, Scope, Status};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GrantDuration {
    RoomSession,
    RobrixSession,
    Always,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoomScope {
    AllRooms,
    /// An empty selection grants nothing; it never means all rooms.
    Selection { rooms: Vec<String>, spaces: Vec<String> },
}

impl RoomScope {
    pub fn room(room: &str) -> Self {
        Self::Selection { rooms: vec![room.to_string()], spaces: Vec::new() }
    }

    fn normalize(&mut self) -> Result<(), String> {
        if let Self::Selection { rooms, spaces } = self {
            for ids in [&mut *rooms, &mut *spaces] {
                for id in ids.iter_mut() { *id = id.trim().to_string(); }
                ids.retain(|id| !id.is_empty());
                ids.sort();
                ids.dedup();
            }
            if rooms.is_empty() && spaces.is_empty() {
                return Err("Choose at least one room or space.".into());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PermissionContext<'a> {
    /// The room that owns the app/agent session; determines room-session expiry.
    pub origin_room: Option<&'a str>,
    /// The actual room being accessed, independently of where the app runs.
    pub target_room: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoomAccess { Read, Write }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyDecision {
    #[default]
    Ask,
    Allow,
    Deny,
}

/// Whether unlisted rooms follow the default or are always inaccessible.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoomPolicyMode {
    #[default]
    Standard,
    WhitelistOnly,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct RoomPolicyModes {
    pub read: RoomPolicyMode,
    pub write: RoomPolicyMode,
}

impl RoomPolicyModes {
    fn get(self, access: RoomAccess) -> RoomPolicyMode {
        match access { RoomAccess::Read => self.read, RoomAccess::Write => self.write }
    }

    fn set(&mut self, access: RoomAccess, mode: RoomPolicyMode) {
        match access { RoomAccess::Read => self.read = mode, RoomAccess::Write => self.write = mode }
    }
}

/// The deciding host rule, borrowing IDs instead of allocating per request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoomPolicyReason<'a> {
    WriteMasterOff,
    GlobalBlock,
    GlobalDefault,
    RoomRule { room: &'a str },
    SpaceRule { space: &'a str, ancestor: bool },
    UnresolvedSpaceHierarchy { space: &'a str, rule: PolicyDecision },
    WhitelistRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoomPolicyEvaluation<'a> {
    pub decision: PolicyDecision,
    pub reason: RoomPolicyReason<'a>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapabilityDecisionReason<'a> {
    Unavailable,
    Undeclared,
    SubjectRestricted,
    RoomPolicy { access: RoomAccess, reason: RoomPolicyReason<'a> },
    PermissionDenied { permission: Permission },
    CapabilityDenied,
    CapabilityGrant,
    ScopedGrant,
    RoomGrant,
    PermissionGrant,
    NormalPermission,
    ApprovalRequired,
    NoPermissionRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapabilityEvaluation<'a> {
    pub effective: Effective,
    pub reason: CapabilityDecisionReason<'a>,
}

impl RoomPolicyReason<'_> {
    /// Explain the controlling setting without revealing protected IDs.
    pub fn public_message(self, access: RoomAccess) -> String {
        let access = if access == RoomAccess::Read { "Read" } else { "Write" };
        let detail = match self {
            Self::WriteMasterOff => return "Room writes are turned off. Review the write switch in Mini Apps > Room and space protection.".into(),
            Self::GlobalBlock => return format!("All room {}s are blocked. Review global access in Mini Apps > Room and space protection.", access.to_ascii_lowercase()),
            Self::GlobalDefault => "access needs approval under the global default",
            Self::RoomRule { .. } => "access is blocked by this room's rule",
            Self::SpaceRule { ancestor: true, .. } => "access is blocked by a containing space's rule",
            Self::SpaceRule { ancestor: false, .. } => "access is blocked by this space's rule",
            Self::UnresolvedSpaceHierarchy { rule: PolicyDecision::Deny, .. } => "access is blocked until the room hierarchy is resolved, because a space block cannot yet be ruled out",
            Self::UnresolvedSpaceHierarchy { .. } => "access is blocked until this room is confirmed to belong to an allowlisted space",
            Self::WhitelistRequired => "access is limited to allowlisted rooms and spaces; this target is not allowlisted",
        };
        format!("{access} {detail}. Open Mini Apps > Inspect protection to review the deciding rule.")
    }
}

impl CapabilityEvaluation<'_> {
    pub fn public_message(self) -> String {
        match self.reason {
            CapabilityDecisionReason::Unavailable => "This capability is not available in this Robrix build.".into(),
            CapabilityDecisionReason::Undeclared => "This app or agent did not declare this capability. Review its manifest or tool profile.".into(),
            CapabilityDecisionReason::SubjectRestricted => "This app or agent is restricted. Review its restriction in App Info.".into(),
            CapabilityDecisionReason::RoomPolicy { access, reason } => reason.public_message(access),
            CapabilityDecisionReason::PermissionDenied { .. } => "This permission group is blocked for this app or agent. Review its permissions in App Info or the AI room panel.".into(),
            CapabilityDecisionReason::CapabilityDenied => "This capability is blocked for this app or agent. Review its capability setting in App Info or the AI room panel.".into(),
            _ => "This request needs approval. Open the app in the foreground and review its permission prompt.".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessPolicy {
    pub read: PolicyDecision,
    pub write: PolicyDecision,
}

impl AccessPolicy {
    fn get(self, access: RoomAccess) -> PolicyDecision {
        match access { RoomAccess::Read => self.read, RoomAccess::Write => self.write }
    }

    fn set(&mut self, access: RoomAccess, decision: PolicyDecision) {
        match access { RoomAccess::Read => self.read = decision, RoomAccess::Write => self.write = decision }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RoomPolicies {
    pub global: AccessPolicy,
    pub rooms: BTreeMap<String, AccessPolicy>,
    pub spaces: BTreeMap<String, AccessPolicy>,
    #[serde(default)]
    pub modes: RoomPolicyModes,
    /// Restored by the master switch; absent in older files means Ask.
    #[serde(default)]
    pub write_when_enabled: Option<PolicyDecision>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopedGrant {
    pub id: u64,
    pub subject: String,
    pub permission: String,
    pub capability: Option<String>,
    pub scope: RoomScope,
    pub duration: GrantDuration,
    pub origin_room: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkScopeKind { ExactUrl, Origin, Host, Domain, AllHosts }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkScope {
    ExactUrl(String),
    /// Scheme, host and effective port, e.g. https://example.com:8443.
    Origin(String),
    /// One exact hostname, on either HTTP(S) scheme and any port.
    Host(String),
    /// This hostname and its subdomains, with a DNS label boundary.
    Domain(String),
    AllHosts,
}

fn parsed_url(value: &str) -> Result<url::Url, String> {
    let mut parsed = url::Url::parse(value).map_err(|_| "Enter a complete HTTP or HTTPS URL.".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none()
        || !parsed.username().is_empty() || parsed.password().is_some()
    {
        return Err("Use an HTTP or HTTPS URL without embedded credentials.".into());
    }
    if let Some(url::Host::Domain(host)) = parsed.host() {
        let host = host.trim_end_matches('.').to_string();
        if host.is_empty() { return Err("The URL must have a hostname.".into()); }
        parsed.set_host(Some(&host)).map_err(|_| "Invalid hostname.".to_string())?;
    }
    parsed.set_fragment(None);
    Ok(parsed)
}

fn normalized_host(value: &str) -> Result<String, String> {
    let value = value.trim().trim_end_matches('.');
    if value.is_empty() || value.contains(['/', '@', '?', '#', '\\']) {
        return Err("Enter a hostname without a path, credentials or port.".into());
    }
    let host = url::Host::parse(value).map_err(|_| "Invalid hostname.".to_string())?;
    Ok(host.to_string().to_ascii_lowercase())
}

impl NetworkScope {
    pub fn from_url(value: &str, kind: NetworkScopeKind) -> Result<Self, String> {
        let url = parsed_url(value)?;
        let host = url.host().unwrap().to_string();
        Ok(match kind {
            NetworkScopeKind::ExactUrl => Self::ExactUrl(url.to_string()),
            NetworkScopeKind::Origin => Self::Origin(url.origin().ascii_serialization()),
            NetworkScopeKind::Host => Self::Host(host),
            NetworkScopeKind::Domain => Self::Domain(host),
            NetworkScopeKind::AllHosts => Self::AllHosts,
        })
    }

    pub fn normalized(&self) -> Result<Self, String> {
        match self {
            Self::ExactUrl(value) => Self::from_url(value, NetworkScopeKind::ExactUrl),
            Self::Origin(value) => {
                let parsed = parsed_url(value)?;
                if parsed.path() != "/" || parsed.query().is_some() {
                    return Err("An origin contains only the scheme, hostname and optional port.".into());
                }
                Ok(Self::Origin(parsed.origin().ascii_serialization()))
            }
            Self::Host(value) => Ok(Self::Host(normalized_host(value)?)),
            Self::Domain(value) => Ok(Self::Domain(normalized_host(value)?)),
            Self::AllHosts => Ok(Self::AllHosts),
        }
    }

    pub fn matches_url(&self, value: &str) -> bool {
        let Ok(url) = parsed_url(value) else { return false };
        let Ok(scope) = self.normalized() else { return false };
        let host = url.host().unwrap().to_string();
        match scope {
            Self::ExactUrl(allowed) => url.as_str() == allowed,
            Self::Origin(allowed) => url.origin().ascii_serialization() == allowed,
            Self::Host(allowed) => host == allowed,
            Self::Domain(allowed) => host == allowed || (
                matches!(url::Host::parse(&allowed), Ok(url::Host::Domain(_)))
                    && host.ends_with(&format!(".{allowed}"))
            ),
            Self::AllHosts => true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkGrant {
    pub id: u64,
    pub subject: String,
    pub network: NetworkScope,
    pub scope: RoomScope,
    pub duration: GrantDuration,
    pub origin_room: Option<String>,
}

/// The read/write boundary touched by a room capability. Mixed operations
/// return Write here; `capability_room_policy` checks both sides.
pub fn capability_room_access(cap: &Capability) -> Option<RoomAccess> {
    if cap.status == Status::RefusedBySwitch || cap.group == Some(Permission::RobrixComposer) {
        return Some(RoomAccess::Write);
    }
    if !matches!(cap.scope, Scope::Room | Scope::MultiRoom | Scope::Space) { return None; }
    match cap.access {
        Access::Read => Some(RoomAccess::Read),
        Access::Write | Access::ReadWrite => Some(RoomAccess::Write),
        Access::Act => None,
    }
}

impl PermissionStore {
    pub(super) fn ensure_room_policies(&mut self) -> &mut RoomPolicies {
        self.room_policies.get_or_insert_with(|| RoomPolicies {
            global: AccessPolicy {
                read: PolicyDecision::Ask,
                write: if self.matrix_write { PolicyDecision::Ask } else { PolicyDecision::Deny },
            },
            ..Default::default()
        })
    }

    pub fn global_policy(&self, access: RoomAccess) -> PolicyDecision {
        self.room_policies.as_ref().map(|p| p.global.get(access)).unwrap_or_else(|| {
            if access == RoomAccess::Write && !self.matrix_write { PolicyDecision::Deny } else { PolicyDecision::Ask }
        })
    }

    pub fn set_global_policy(&mut self, access: RoomAccess, decision: PolicyDecision) {
        if access == RoomAccess::Write && decision == PolicyDecision::Deny {
            self.set_matrix_write(false);
            return;
        }
        self.ensure_room_policies().global.set(access, decision);
        self.ensure_room_policies().modes.set(access, RoomPolicyMode::Standard);
        if access == RoomAccess::Write { self.matrix_write = decision != PolicyDecision::Deny; }
    }

    pub fn policy_mode(&self, access: RoomAccess) -> RoomPolicyMode {
        self.room_policies.as_ref().map(|p| p.modes.get(access)).unwrap_or_default()
    }

    /// Choosing a mode is an explicit replacement for the global block.
    /// The master switch uses its own setter to preserve this choice.
    pub fn set_policy_mode(&mut self, access: RoomAccess, mode: RoomPolicyMode) {
        self.set_global_policy(access, PolicyDecision::Ask);
        self.ensure_room_policies().modes.set(access, mode);
    }

    pub fn write_policy_when_enabled(&self) -> PolicyDecision {
        let current = self.global_policy(RoomAccess::Write);
        if current != PolicyDecision::Deny { return current; }
        match self.room_policies.as_ref().and_then(|p| p.write_when_enabled) {
            Some(PolicyDecision::Allow) => PolicyDecision::Allow,
            _ => PolicyDecision::Ask,
        }
    }

    pub fn set_room_policy(&mut self, room: &str, access: RoomAccess, decision: PolicyDecision) {
        let rules = &mut self.ensure_room_policies().rooms;
        rules.entry(room.to_string()).or_default().set(access, decision);
        rules.retain(|_, policy| *policy != AccessPolicy::default());
    }

    pub fn set_space_policy(&mut self, space: &str, access: RoomAccess, decision: PolicyDecision) {
        let rules = &mut self.ensure_room_policies().spaces;
        rules.entry(space.to_string()).or_default().set(access, decision);
        rules.retain(|_, policy| *policy != AccessPolicy::default());
    }

    pub fn room_rules(&self) -> &BTreeMap<String, AccessPolicy> {
        static EMPTY: std::sync::LazyLock<BTreeMap<String, AccessPolicy>> = std::sync::LazyLock::new(BTreeMap::new);
        self.room_policies.as_ref().map(|p| &p.rooms).unwrap_or(&EMPTY)
    }

    pub fn space_rules(&self) -> &BTreeMap<String, AccessPolicy> {
        static EMPTY: std::sync::LazyLock<BTreeMap<String, AccessPolicy>> = std::sync::LazyLock::new(BTreeMap::new);
        self.room_policies.as_ref().map(|p| &p.spaces).unwrap_or(&EMPTY)
    }

    /// Supply the complete trusted ancestor set. An empty set records that
    /// the room has been resolved and belongs to no spaces.
    pub fn set_room_spaces(&mut self, room: &str, spaces: Vec<String>) {
        self.room_spaces.insert(room.to_string(), spaces.into_iter().collect());
    }

    pub fn clear_room_spaces(&mut self) { self.room_spaces.clear(); }

    /// Keep resolving configured spaces even after the user leaves them:
    /// leaving a space must not silently remove protection from its rooms.
    pub fn configured_space_ids(&self) -> BTreeSet<String> {
        let mut spaces: BTreeSet<String> = self.space_rules().keys().cloned().collect();
        let scopes = self.scoped.iter().chain(&self.session_scoped).map(|grant| &grant.scope)
            .chain(self.network.iter().chain(&self.session_network).map(|grant| &grant.scope));
        for scope in scopes {
            if let RoomScope::Selection { spaces: selected, .. } = scope {
                spaces.extend(selected.iter().cloned());
            }
        }
        spaces
    }

    pub fn room_policy(&self, room: Option<&str>, access: RoomAccess) -> PolicyDecision {
        self.room_policy_evaluation(room, access).decision
    }

    /// List every host block for the trusted inspector, without changing
    /// the allocation-free enforcement path's first deciding rule.
    pub fn room_policy_blockers<'a>(&'a self, room: Option<&'a str>, access: RoomAccess) -> Vec<RoomPolicyReason<'a>> {
        use RoomPolicyReason::*;
        let mut blockers = Vec::new();
        if self.global_policy(access) == PolicyDecision::Deny {
            blockers.push(if access == RoomAccess::Write { WriteMasterOff } else { GlobalBlock });
        }
        if let Some((room, policy)) = room.and_then(|id| self.room_rules().get_key_value(id)) {
            if policy.get(access) == PolicyDecision::Deny { blockers.push(RoomRule { room }); }
        }
        let ancestors = room.and_then(|id| self.room_spaces.get(id));
        for (space, policy) in self.space_rules() {
            if policy.get(access) != PolicyDecision::Deny { continue; }
            if room == Some(space.as_str()) || ancestors.is_some_and(|set| set.contains(space)) {
                blockers.push(SpaceRule { space, ancestor: room != Some(space.as_str()) });
            } else if ancestors.is_none() {
                blockers.push(UnresolvedSpaceHierarchy { space, rule: PolicyDecision::Deny });
            }
        }
        if blockers.is_empty() {
            let evaluation = self.room_policy_evaluation(room, access);
            if evaluation.decision == PolicyDecision::Deny { blockers.push(evaluation.reason); }
        }
        blockers
    }

    /// Use the same evaluation for enforcement and the protection inspector.
    /// Hard blocks outrank every allowance, including a direct room rule.
    pub fn room_policy_evaluation<'a>(&'a self, room: Option<&'a str>, access: RoomAccess) -> RoomPolicyEvaluation<'a> {
        use RoomPolicyReason::*;
        let result = |decision, reason| RoomPolicyEvaluation { decision, reason };
        let global = self.global_policy(access);
        if global == PolicyDecision::Deny {
            return result(global, if access == RoomAccess::Write { WriteMasterOff } else { GlobalBlock });
        }
        let mut allow = None;
        let ancestors = room.and_then(|r| self.room_spaces.get(r));
        if let Some((room, policy)) = room.and_then(|r| self.room_rules().get_key_value(r)) {
            match policy.get(access) {
                PolicyDecision::Deny => return result(PolicyDecision::Deny, RoomRule { room }),
                PolicyDecision::Allow => allow = Some(RoomRule { room }),
                PolicyDecision::Ask => {}
            }
        }
        let mut unresolved_block = None;
        let mut unresolved_allow = None;
        for (space, policy) in self.space_rules() {
            if room == Some(space.as_str()) || ancestors.is_some_and(|set| set.contains(space)) {
                let reason = SpaceRule { space, ancestor: room != Some(space.as_str()) };
                match policy.get(access) {
                    PolicyDecision::Deny => return result(PolicyDecision::Deny, reason),
                    PolicyDecision::Allow => { if allow.is_none() { allow = Some(reason); } }
                    PolicyDecision::Ask => {}
                }
            } else if ancestors.is_none() {
                match policy.get(access) {
                    PolicyDecision::Deny => { if unresolved_block.is_none() { unresolved_block = Some(space); } }
                    PolicyDecision::Allow => { if unresolved_allow.is_none() { unresolved_allow = Some(space); } }
                    PolicyDecision::Ask => {}
                }
            }
        }
        // A missing hierarchy must never bypass a protected space during
        // startup or reconnect. Once resolved, unrelated rooms stay usable.
        if let Some(space) = unresolved_block {
            return result(PolicyDecision::Deny, UnresolvedSpaceHierarchy { space, rule: PolicyDecision::Deny });
        }
        if let Some(reason) = allow { return result(PolicyDecision::Allow, reason); }
        if self.policy_mode(access) == RoomPolicyMode::WhitelistOnly {
            let reason = unresolved_allow.map(|space| UnresolvedSpaceHierarchy { space, rule: PolicyDecision::Allow })
                .unwrap_or(WhitelistRequired);
            return result(PolicyDecision::Deny, reason);
        }
        result(global, GlobalDefault)
    }

    pub fn capability_room_policy(&self, cap: &Capability, context: PermissionContext<'_>) -> PolicyDecision {
        self.capability_room_evaluation(cap, context).map(|(_, evaluation)| evaluation.decision).unwrap_or(PolicyDecision::Ask)
    }

    pub fn capability_room_evaluation<'a>(&'a self, cap: &Capability, context: PermissionContext<'a>)
        -> Option<(RoomAccess, RoomPolicyEvaluation<'a>)>
    {
        let access = capability_room_access(cap)?;
        let room = context.target_room.or(context.origin_room);
        let decision = self.room_policy_evaluation(room, access);
        if cap.access != Access::ReadWrite || decision.decision == PolicyDecision::Deny { return Some((access, decision)); }
        let read = self.room_policy_evaluation(room, RoomAccess::Read);
        if read.decision != PolicyDecision::Allow { Some((RoomAccess::Read, read)) }
        else { Some((access, decision)) }
    }

    fn scope_matches(&self, scope: &RoomScope, context: PermissionContext<'_>) -> bool {
        match scope {
            RoomScope::AllRooms => true,
            RoomScope::Selection { rooms, spaces } => {
                let Some(room) = context.target_room.or(context.origin_room) else { return false };
                rooms.iter().any(|id| id == room) || spaces.iter().any(|space| {
                    space == room || self.room_spaces.get(room).is_some_and(|ancestors| ancestors.contains(space))
                })
            }
        }
    }

    fn lifetime_matches(duration: GrantDuration, origin: Option<&str>, context: PermissionContext<'_>) -> bool {
        duration != GrantDuration::RoomSession || (origin.is_some() && origin == context.origin_room)
    }

    fn next_grant_id(&mut self) -> u64 {
        let highest = self.scoped.iter().chain(&self.session_scoped).map(|g| g.id)
            .chain(self.network.iter().chain(&self.session_network).map(|g| g.id))
            .max().unwrap_or(0);
        self.grant_serial = self.grant_serial.max(highest).saturating_add(1);
        self.grant_serial
    }

    fn grant_origin(duration: GrantDuration, origin: Option<&str>) -> Result<Option<String>, String> {
        if duration == GrantDuration::RoomSession {
            Ok(Some(origin.filter(|s| !s.is_empty()).ok_or("A room session needs an attached room.")?.to_string()))
        } else { Ok(None) }
    }

    pub fn grant_scoped(&mut self, subject: &str, permission: Permission, capability: Option<&str>, mut scope: RoomScope,
        duration: GrantDuration, origin_room: Option<&str>) -> Result<u64, String>
    {
        scope.normalize()?;
        if let Some(id) = capability {
            if crate::capabilities::by_id(id).is_none_or(|cap| cap.group != Some(permission)) {
                return Err("The capability does not belong to this permission.".into());
            }
        }
        let grant = ScopedGrant {
            id: self.next_grant_id(), subject: subject.to_string(), permission: permission.as_str().to_string(),
            capability: capability.map(str::to_string), scope, duration,
            origin_room: Self::grant_origin(duration, origin_room)?,
        };
        let id = grant.id;
        self.scoped_ask.entry(subject.to_string()).or_default()
            .insert(capability.unwrap_or(permission.as_str()).to_string());
        if duration == GrantDuration::Always { self.scoped.push(grant); } else { self.session_scoped.push(grant); }
        Ok(id)
    }

    pub fn scoped_grants(&self, subject: &str) -> Vec<&ScopedGrant> {
        self.scoped.iter().chain(&self.session_scoped).filter(|g| g.subject == subject).collect()
    }

    pub fn remove_scoped_grant(&mut self, id: u64) -> bool {
        self.request_once.remove(&id);
        let before = self.scoped.len() + self.session_scoped.len();
        self.scoped.retain(|g| g.id != id);
        self.session_scoped.retain(|g| g.id != id);
        before != self.scoped.len() + self.session_scoped.len()
    }

    pub fn revoke_scoped_grant(&mut self, id: u64) -> bool { self.remove_scoped_grant(id) }

    /// Mark only the temporary grant created for an Allow Once replay. The
    /// worker's cloned proof may finish that request after the temporary
    /// grant is removed; normal session/persistent grants never get this.
    pub fn mark_request_once(&mut self, id: u64) {
        if self.session_scoped.iter().any(|g| g.id == id) {
            self.request_once.insert(id);
        }
    }

    pub fn has_request_once(&self, subject: &str, capability: &str, context: PermissionContext<'_>) -> bool {
        let Some(cap) = crate::capabilities::by_id(capability) else { return false };
        let Some(permission) = cap.group else { return false };
        self.session_scoped.iter().any(|g| {
            self.request_once.contains(&g.id) && g.subject == subject && g.permission == permission.as_str()
                && (g.capability.is_none() || g.capability.as_deref() == Some(capability))
                && Self::lifetime_matches(g.duration, g.origin_room.as_deref(), context)
                && self.scope_matches(&g.scope, context)
        })
    }

    pub fn grant_scoped_tool(&mut self, subject: &str, tool: &str, content_hash: &str, mut scope: RoomScope,
        duration: GrantDuration, origin_room: Option<&str>) -> Result<u64, String>
    {
        scope.normalize()?;
        let key = serde_json::to_string(&(tool, content_hash)).map_err(|e| e.to_string())?;
        let grant = ScopedGrant {
            id: self.next_grant_id(), subject: subject.to_string(), permission: Permission::McpTools.as_str().to_string(),
            capability: Some(format!("tool:{key}")), scope, duration,
            origin_room: Self::grant_origin(duration, origin_room)?,
        };
        let id = grant.id;
        if duration == GrantDuration::Always { self.scoped.push(grant); } else { self.session_scoped.push(grant); }
        Ok(id)
    }

    pub fn has_scoped_tool_grant(&self, subject: &str, tool: &str, content_hash: &str, context: PermissionContext<'_>) -> bool {
        if self.is_tool_denied(subject, tool) || self.is_restricted(subject)
            || self.state(subject, Permission::McpTools) == GrantState::Denied { return false; }
        let Ok(key) = serde_json::to_string(&(tool, content_hash)) else { return false };
        let key = format!("tool:{key}");
        self.scoped.iter().chain(&self.session_scoped).any(|g| {
            g.subject == subject && g.permission == Permission::McpTools.as_str()
                && g.capability.as_deref() == Some(key.as_str())
                && Self::lifetime_matches(g.duration, g.origin_room.as_deref(), context)
                && self.scope_matches(&g.scope, context)
        })
    }

    pub fn has_scoped_grant(&self, subject: &str, permission: Permission, capability: Option<&str>, context: PermissionContext<'_>) -> bool {
        self.scoped.iter().chain(&self.session_scoped).any(|g| {
            g.subject == subject && g.permission == permission.as_str()
                && (g.capability.is_none() || g.capability.as_deref() == capability)
                && Self::lifetime_matches(g.duration, g.origin_room.as_deref(), context)
                && self.scope_matches(&g.scope, context)
        })
    }

    pub fn effective_for_in_context(&self, subject: &str, declares: impl Fn(Permission) -> bool,
        permission: Permission, context: PermissionContext<'_>) -> Effective
    {
        let effective = self.scoped_normal_default(subject, permission, None, self.effective_for(subject, declares, permission));
        if effective == Effective::NeedsPrompt && self.has_scoped_grant(subject, permission, None, context) {
            Effective::Granted
        } else { effective }
    }

    fn scoped_normal_default(&self, subject: &str, permission: Permission, capability: Option<&str>, effective: Effective) -> Effective {
        if effective != Effective::Granted || permission.tier() != Tier::Normal
            || self.state(subject, permission) != GrantState::Ask || self.has_once(subject, permission)
        { return effective; }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        if self.timed_until(subject, permission, now).is_some() { return effective; }
        let narrowed = self.scoped_ask.get(subject).is_some_and(|keys| {
            keys.contains(permission.as_str()) || capability.is_some_and(|id| keys.contains(id))
        }) || self.scoped.iter().chain(&self.session_scoped).any(|g| {
            g.subject == subject && g.permission == permission.as_str()
                && (g.capability.is_none() || g.capability.as_deref() == capability)
        });
        if narrowed { Effective::NeedsPrompt } else { effective }
    }

    pub fn effective_capability_in_context(&self, manifest: &MiniAppManifest, cap: &Capability, context: PermissionContext<'_>) -> Effective {
        self.effective_capability_for_in_context(&manifest.id, |p| manifest.declares(p), |c| manifest.declares_capability(c), cap, context)
    }

    pub fn effective_capability_for_in_context(&self, subject: &str, declares_perm: impl Fn(Permission) -> bool,
        declares_cap: impl Fn(&Capability) -> bool, cap: &Capability, context: PermissionContext<'_>) -> Effective
    {
        self.capability_evaluation_for_in_context(subject, declares_perm, declares_cap, cap, context).effective
    }

    pub fn capability_evaluation_for_in_context<'a>(&'a self, subject: &str, declares_perm: impl Fn(Permission) -> bool,
        declares_cap: impl Fn(&Capability) -> bool, cap: &Capability, context: PermissionContext<'a>) -> CapabilityEvaluation<'a>
    {
        use CapabilityDecisionReason::*;
        let result = |effective, reason| CapabilityEvaluation { effective, reason };
        if !cap.is_available() { return result(Effective::Undeclared, Unavailable); }
        if !declares_cap(cap) { return result(Effective::Undeclared, Undeclared); }
        if self.is_restricted(subject) { return result(Effective::Denied, SubjectRestricted); }
        let policy = self.capability_room_evaluation(cap, context);
        if let Some((access, evaluation)) = policy {
            if evaluation.decision == PolicyDecision::Deny {
                return result(Effective::Denied, RoomPolicy { access, reason: evaluation.reason });
            }
        }
        let Some(group) = cap.group else { return result(Effective::Granted, NoPermissionRequired) };
        let base = self.scoped_normal_default(subject, group, Some(cap.id), self.effective_for(subject, &declares_perm, group));
        if base == Effective::Undeclared { return result(base, Undeclared); }
        if base == Effective::Denied { return result(base, PermissionDenied { permission: group }); }
        match self.capability_state(subject, cap.id) {
            GrantState::Denied => result(Effective::Denied, CapabilityDenied),
            GrantState::Granted => result(Effective::Granted, CapabilityGrant),
            GrantState::Ask => {
                if let Some((access, evaluation)) = policy {
                    if evaluation.decision == PolicyDecision::Allow {
                        return result(Effective::Granted, RoomPolicy { access, reason: evaluation.reason });
                    }
                }
                if self.has_scoped_grant(subject, group, Some(cap.id), context) { return result(Effective::Granted, ScopedGrant); }
                if context.target_room.or(context.origin_room).is_some_and(|room| {
                    (group == Permission::MatrixRoomsRead && self.is_room_read_allowed(subject, room))
                        || (group == Permission::MatrixRoomsSend && self.is_room_send_allowed(subject, room))
                }) { return result(Effective::Granted, RoomGrant); }
                let reason = if base == Effective::NeedsPrompt { ApprovalRequired }
                    else if self.state(subject, group) == GrantState::Ask && group.tier() == Tier::Normal
                        && !self.has_once(subject, group)
                        && self.timed_until(subject, group, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)).is_none()
                    { NormalPermission } else { PermissionGrant };
                result(base, reason)
            }
        }
    }

    pub fn has_scoped_collection_consent(&self, subject: &str, cap: &Capability, context: PermissionContext<'_>) -> bool {
        let Some(group) = cap.group else { return false };
        self.scoped.iter().chain(&self.session_scoped).any(|g| {
            g.subject == subject && g.permission == group.as_str()
                && (g.capability.is_none() || g.capability.as_deref() == Some(cap.id))
                && Self::lifetime_matches(g.duration, g.origin_room.as_deref(), context)
                && match &g.scope {
                    RoomScope::AllRooms => true,
                    RoomScope::Selection { rooms, spaces } => !rooms.is_empty() || !spaces.is_empty(),
                }
        }) || (group == Permission::MatrixRoomsRead && self.read_rooms.get(subject).is_some_and(|rooms| !rooms.is_empty()))
    }

    /// Admits only read-only collection orchestration. A grant for any subset
    /// can start the query, but EVERY returned/accessed room must subsequently
    /// pass `effective_capability_for_in_context` with its actual target ID.
    /// Space denials are deliberately enforced at that target boundary.
    pub fn effective_collection_capability_for_in_context(&self, subject: &str, declares_perm: impl Fn(Permission) -> bool,
        declares_cap: impl Fn(&Capability) -> bool, cap: &Capability, context: PermissionContext<'_>) -> Effective
    {
        if cap.access != Access::Read || !matches!(cap.scope, Scope::MultiRoom | Scope::Space) {
            return self.effective_capability_for_in_context(subject, declares_perm, declares_cap, cap, context);
        }
        if !cap.is_available() || !declares_cap(cap) { return Effective::Undeclared; }
        if self.is_restricted(subject) || self.global_policy(RoomAccess::Read) == PolicyDecision::Deny { return Effective::Denied; }
        let has_allowance = self.room_rules().values().chain(self.space_rules().values()).any(|rule| rule.read == PolicyDecision::Allow);
        if self.policy_mode(RoomAccess::Read) == RoomPolicyMode::WhitelistOnly && !has_allowance { return Effective::Denied; }
        let Some(group) = cap.group else { return Effective::Granted };
        let base = self.scoped_normal_default(subject, group, Some(cap.id), self.effective_for(subject, &declares_perm, group));
        if matches!(base, Effective::Undeclared | Effective::Denied) { return base; }
        match self.capability_state(subject, cap.id) {
            GrantState::Denied => Effective::Denied,
            GrantState::Granted => Effective::Granted,
            GrantState::Ask => {
                let whitelist = self.global_policy(RoomAccess::Read) == PolicyDecision::Allow
                    || has_allowance;
                if whitelist || self.has_scoped_collection_consent(subject, cap, context) { Effective::Granted } else { base }
            }
        }
    }

    pub fn effective_collection_capability_in_context(&self, manifest: &MiniAppManifest, cap: &Capability, context: PermissionContext<'_>) -> Effective {
        self.effective_collection_capability_for_in_context(&manifest.id, |p| manifest.declares(p), |c| manifest.declares_capability(c), cap, context)
    }

    pub fn allow_network(&mut self, subject: &str, network: NetworkScope, mut scope: RoomScope,
        duration: GrantDuration, origin_room: Option<&str>) -> Result<u64, String>
    {
        scope.normalize()?;
        let grant = NetworkGrant {
            id: self.next_grant_id(), subject: subject.to_string(), network: network.normalized()?, scope, duration,
            origin_room: Self::grant_origin(duration, origin_room)?,
        };
        let id = grant.id;
        if duration == GrantDuration::Always { self.network.push(grant); } else { self.session_network.push(grant); }
        Ok(id)
    }

    pub fn network_grants(&self, subject: &str) -> Vec<&NetworkGrant> {
        self.network.iter().chain(&self.session_network).filter(|g| g.subject == subject).collect()
    }

    pub fn remove_network_grant(&mut self, id: u64) -> bool {
        self.request_once.remove(&id);
        let before = self.network.len() + self.session_network.len();
        self.network.retain(|g| g.id != id);
        self.session_network.retain(|g| g.id != id);
        before != self.network.len() + self.session_network.len()
    }

    pub fn revoke_network_grant(&mut self, id: u64) -> bool { self.remove_network_grant(id) }

    /// An exact pending network request may finish after its temporary
    /// grant is removed. Ordinary revoked network grants are never receipts.
    pub fn mark_network_request_once(&mut self, id: u64) {
        if self.session_network.iter().any(|grant| grant.id == id) {
            self.request_once.insert(id);
        }
    }

    pub fn has_network_request_once(&self, subject: &str, value: &str, context: PermissionContext<'_>) -> bool {
        let Ok(url) = parsed_url(value) else { return false };
        self.session_network.iter().any(|grant| {
            self.request_once.contains(&grant.id) && grant.subject == subject
                && Self::lifetime_matches(grant.duration, grant.origin_room.as_deref(), context)
                && self.scope_matches(&grant.scope, context) && grant.network.matches_url(url.as_str())
        })
    }

    pub fn is_url_allowed(&self, subject: &str, value: &str, context: PermissionContext<'_>) -> bool {
        if self.is_restricted(subject) || self.state(subject, Permission::Network) == GrantState::Denied
            || self.capability_state(subject, "network.http") == GrantState::Denied { return false; }
        let Ok(url) = parsed_url(value) else { return false };
        if self.is_host_allowed(subject, url.host_str().unwrap_or_default()) { return true; }
        self.network.iter().chain(&self.session_network).any(|g| {
            g.subject == subject && Self::lifetime_matches(g.duration, g.origin_room.as_deref(), context)
                && self.scope_matches(&g.scope, context) && g.network.matches_url(url.as_str())
        })
    }

    /// Closing a room ends only that room's session grants. Restarting an
    /// isolate uses `clear_once_for`, leaving both session lifetimes intact.
    pub fn clear_room_session(&mut self, room: &str) -> bool {
        let before = self.session_scoped.len() + self.session_network.len();
        self.session_scoped.retain(|g| !(g.duration == GrantDuration::RoomSession && g.origin_room.as_deref() == Some(room)));
        self.session_network.retain(|g| !(g.duration == GrantDuration::RoomSession && g.origin_room.as_deref() == Some(room)));
        before != self.session_scoped.len() + self.session_network.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context<'a>(origin: &'a str, target: &'a str) -> PermissionContext<'a> {
        PermissionContext { origin_room: Some(origin), target_room: Some(target) }
    }

    fn effective(store: &PermissionStore, cap: &str, ctx: PermissionContext<'_>) -> Effective {
        store.effective_capability_for_in_context("app", |_| true, |_| true, crate::capabilities::by_id(cap).unwrap(), ctx)
    }

    #[test]
    fn global_and_space_denies_override_room_whitelists() {
        let mut store = PermissionStore::default();
        store.set_room_spaces("!test", vec!["!parent".into(), "!ancestor".into()]);
        store.set_room_policy("!test", RoomAccess::Write, PolicyDecision::Allow);
        assert_eq!(store.room_policy(Some("!test"), RoomAccess::Write), PolicyDecision::Deny);
        store.set_global_policy(RoomAccess::Write, PolicyDecision::Ask);
        assert_eq!(store.room_policy(Some("!test"), RoomAccess::Write), PolicyDecision::Allow);
        store.set_space_policy("!ancestor", RoomAccess::Write, PolicyDecision::Deny);
        assert_eq!(store.room_policy(Some("!test"), RoomAccess::Write), PolicyDecision::Deny);
        store.set_global_policy(RoomAccess::Write, PolicyDecision::Allow);
        assert_eq!(store.room_policy(Some("!test"), RoomAccess::Write), PolicyDecision::Deny);
        assert_eq!(store.room_policy(Some("!test"), RoomAccess::Read), PolicyDecision::Ask);
    }

    #[test]
    fn whitelist_modes_are_independent_and_cannot_be_bypassed_by_subject_grants() {
        let mut store = PermissionStore::default();
        store.set_global_policy(RoomAccess::Read, PolicyDecision::Allow);
        store.set_global_policy(RoomAccess::Write, PolicyDecision::Ask);
        store.set_policy_mode(RoomAccess::Read, RoomPolicyMode::WhitelistOnly);
        store.set_room_policy("!selected", RoomAccess::Read, PolicyDecision::Allow);
        store.set("app", Permission::MatrixRoomsRead, GrantState::Granted);
        store.set_capability("app", "matrix.rooms.messages.read", GrantState::Granted);
        store.grant_scoped("app", Permission::MatrixRoomsRead, None, RoomScope::AllRooms, GrantDuration::Always, None).unwrap();
        assert_eq!(effective(&store, "matrix.rooms.messages.read", context("!selected", "!selected")), Effective::Granted);
        assert_eq!(effective(&store, "matrix.rooms.messages.read", context("!selected", "!unlisted")), Effective::Denied);
        assert_eq!(store.room_policy(Some("!unlisted"), RoomAccess::Write), PolicyDecision::Ask);
        store.set_policy_mode(RoomAccess::Write, RoomPolicyMode::WhitelistOnly);
        assert_eq!(store.room_policy(Some("!selected"), RoomAccess::Write), PolicyDecision::Deny);
        store.set_room_policy("!selected", RoomAccess::Write, PolicyDecision::Allow);
        assert_eq!(store.room_policy(Some("!selected"), RoomAccess::Write), PolicyDecision::Allow);
        store.set_global_policy(RoomAccess::Read, PolicyDecision::Ask);
        assert_eq!(store.policy_mode(RoomAccess::Read), RoomPolicyMode::Standard);
        assert_eq!(store.room_policy(Some("!unlisted"), RoomAccess::Read), PolicyDecision::Ask);
        assert_eq!(store.policy_mode(RoomAccess::Write), RoomPolicyMode::WhitelistOnly);
    }

    #[test]
    fn whitelist_spaces_include_nested_rooms_but_never_override_hard_blocks() {
        let mut store = PermissionStore::default();
        store.set_policy_mode(RoomAccess::Read, RoomPolicyMode::WhitelistOnly);
        store.set_space_policy("!selected-space", RoomAccess::Read, PolicyDecision::Allow);
        store.set_room_spaces("!nested", vec!["!subspace".into(), "!selected-space".into()]);
        store.set_room_spaces("!outside", vec![]);
        assert_eq!(store.room_policy(Some("!selected-space"), RoomAccess::Read), PolicyDecision::Allow);
        assert_eq!(store.room_policy(Some("!nested"), RoomAccess::Read), PolicyDecision::Allow);
        assert_eq!(store.room_policy(Some("!outside"), RoomAccess::Read), PolicyDecision::Deny);
        assert_eq!(store.room_policy(Some("!unresolved"), RoomAccess::Read), PolicyDecision::Deny);
        store.set_space_policy("!subspace", RoomAccess::Read, PolicyDecision::Deny);
        store.set_room_policy("!nested", RoomAccess::Read, PolicyDecision::Allow);
        assert_eq!(store.room_policy(Some("!nested"), RoomAccess::Read), PolicyDecision::Deny);
        store.set_space_policy("!subspace", RoomAccess::Read, PolicyDecision::Ask);
        store.set_room_policy("!nested", RoomAccess::Read, PolicyDecision::Deny);
        assert_eq!(store.room_policy(Some("!nested"), RoomAccess::Read), PolicyDecision::Deny);
        store.set_global_policy(RoomAccess::Read, PolicyDecision::Deny);
        assert_eq!(store.room_policy(Some("!selected-space"), RoomAccess::Read), PolicyDecision::Deny);
    }

    #[test]
    fn write_master_restores_default_and_mode_after_restart_without_changing_rules() {
        for (decision, mode) in [
            (PolicyDecision::Ask, RoomPolicyMode::Standard),
            (PolicyDecision::Allow, RoomPolicyMode::Standard),
            (PolicyDecision::Ask, RoomPolicyMode::WhitelistOnly),
        ] {
            let mut store = PermissionStore::default();
            store.set_global_policy(RoomAccess::Write, decision);
            if mode == RoomPolicyMode::WhitelistOnly { store.set_policy_mode(RoomAccess::Write, mode); }
            store.set_room_policy("!test", RoomAccess::Write, PolicyDecision::Allow);
            store.set_space_policy("!protected", RoomAccess::Write, PolicyDecision::Deny);
            store.set_policy_mode(RoomAccess::Read, RoomPolicyMode::WhitelistOnly);
            store.set_matrix_write(false);
            store.set_matrix_write(false);
            assert!(!store.matrix_write());
            assert_eq!(store.room_policy(Some("!test"), RoomAccess::Write), PolicyDecision::Deny);
            assert_eq!(store.write_policy_when_enabled(), decision);
            let mut restored: PermissionStore = serde_json::from_value(serde_json::to_value(&store).unwrap()).unwrap();
            restored.migrate();
            restored.set_matrix_write(true);
            restored.set_matrix_write(true);
            assert!(restored.matrix_write());
            assert_eq!(restored.global_policy(RoomAccess::Write), decision);
            assert_eq!(restored.policy_mode(RoomAccess::Write), mode);
            assert_eq!(restored.policy_mode(RoomAccess::Read), RoomPolicyMode::WhitelistOnly);
            assert_eq!(restored.room_rules(), store.room_rules());
            assert_eq!(restored.space_rules(), store.space_rules());
            restored.set_room_spaces("!test", vec![]);
            assert_eq!(restored.room_policy(Some("!test"), RoomAccess::Write), PolicyDecision::Allow);
            assert_eq!(restored.room_policy(Some("!protected"), RoomAccess::Write), PolicyDecision::Deny);
        }
    }

    #[test]
    fn old_global_blocks_are_not_reinterpreted_as_whitelists() {
        let mut store: PermissionStore = serde_json::from_value(serde_json::json!({
            "schema": 2, "grants": {}, "matrix_write": true,
            "room_policies": {
                "global": {"read":"Deny", "write":"Deny"},
                "rooms": {"!selected": {"read":"Allow", "write":"Allow"}}, "spaces": {}
            }
        })).unwrap();
        store.migrate();
        for access in [RoomAccess::Read, RoomAccess::Write] {
            assert_eq!(store.policy_mode(access), RoomPolicyMode::Standard);
            assert_eq!(store.room_policy(Some("!selected"), access), PolicyDecision::Deny);
        }
        assert!(!store.matrix_write());
        assert_eq!(store.write_policy_when_enabled(), PolicyDecision::Ask);
        store.set_matrix_write(true);
        assert_eq!(store.room_policy(Some("!selected"), RoomAccess::Write), PolicyDecision::Allow);
        assert_eq!(store.room_policy(Some("!other"), RoomAccess::Write), PolicyDecision::Ask);
        assert_eq!(store.room_policy(Some("!selected"), RoomAccess::Read), PolicyDecision::Deny);
    }

    #[test]
    fn policy_explanations_identify_the_enforced_rule_and_unresolved_hierarchy() {
        let mut store = PermissionStore::default();
        assert_eq!(store.room_policy_evaluation(Some("!room"), RoomAccess::Write).reason, RoomPolicyReason::WriteMasterOff);
        store.set_policy_mode(RoomAccess::Read, RoomPolicyMode::WhitelistOnly);
        assert_eq!(store.room_policy_evaluation(Some("!room"), RoomAccess::Read).reason, RoomPolicyReason::WhitelistRequired);
        store.set_space_policy("!parent", RoomAccess::Read, PolicyDecision::Allow);
        assert_eq!(store.room_policy_evaluation(Some("!room"), RoomAccess::Read).reason,
            RoomPolicyReason::UnresolvedSpaceHierarchy { space: "!parent", rule: PolicyDecision::Allow });
        store.set_room_spaces("!room", vec!["!parent".into()]);
        assert_eq!(store.room_policy_evaluation(Some("!room"), RoomAccess::Read), RoomPolicyEvaluation {
            decision: PolicyDecision::Allow, reason: RoomPolicyReason::SpaceRule { space: "!parent", ancestor: true },
        });
        store.set_room_policy("!room", RoomAccess::Read, PolicyDecision::Allow);
        assert_eq!(store.room_policy_evaluation(Some("!room"), RoomAccess::Read).reason, RoomPolicyReason::RoomRule { room: "!room" });
        store.set_space_policy("!parent", RoomAccess::Read, PolicyDecision::Deny);
        assert_eq!(store.room_policy_evaluation(Some("!room"), RoomAccess::Read), RoomPolicyEvaluation {
            decision: PolicyDecision::Deny, reason: RoomPolicyReason::SpaceRule { space: "!parent", ancestor: true },
        });
        store.clear_room_spaces();
        assert_eq!(store.room_policy_evaluation(Some("!room"), RoomAccess::Read).reason,
            RoomPolicyReason::UnresolvedSpaceHierarchy { space: "!parent", rule: PolicyDecision::Deny });
    }

    #[test]
    fn capability_explanations_keep_app_denials_above_room_allowances() {
        let mut store = PermissionStore::default();
        let cap = crate::capabilities::by_id("matrix.room.messages.read").unwrap();
        let ctx = context("!room", "!room");
        store.set_room_policy("!room", RoomAccess::Read, PolicyDecision::Allow);
        store.set("app", Permission::MatrixRoomRead, GrantState::Denied);
        let decision = store.capability_evaluation_for_in_context("app", |_| true, |_| true, cap, ctx);
        assert_eq!(decision.effective, Effective::Denied);
        assert_eq!(decision.reason, CapabilityDecisionReason::PermissionDenied { permission: Permission::MatrixRoomRead });
        store.set("app", Permission::MatrixRoomRead, GrantState::Ask);
        store.set_capability("app", cap.id, GrantState::Denied);
        assert_eq!(store.capability_evaluation_for_in_context("app", |_| true, |_| true, cap, ctx).reason,
            CapabilityDecisionReason::CapabilityDenied);
        store.set_capability("app", cap.id, GrantState::Ask);
        assert_eq!(store.capability_evaluation_for_in_context("app", |_| true, |_| true, cap, ctx).reason,
            CapabilityDecisionReason::RoomPolicy { access: RoomAccess::Read, reason: RoomPolicyReason::RoomRule { room: "!room" } });
    }

    #[test]
    fn inspector_lists_all_hard_blocks_and_public_messages_hide_their_ids() {
        let mut store = PermissionStore::default();
        store.set_room_spaces("!private-room", vec!["!secret-one".into(), "!secret-two".into()]);
        store.set_room_policy("!private-room", RoomAccess::Read, PolicyDecision::Allow);
        for space in ["!secret-one", "!secret-two"] { store.set_space_policy(space, RoomAccess::Read, PolicyDecision::Deny); }
        let blockers = store.room_policy_blockers(Some("!private-room"), RoomAccess::Read);
        assert_eq!(blockers, vec![
            RoomPolicyReason::SpaceRule { space: "!secret-one", ancestor: true },
            RoomPolicyReason::SpaceRule { space: "!secret-two", ancestor: true },
        ]);
        for reason in blockers {
            let message = reason.public_message(RoomAccess::Read);
            assert!(message.contains("Read access") && message.contains("containing space"));
            assert!(!message.contains("!secret") && !message.contains("!private"));
        }
        store.set_space_policy("!secret-one", RoomAccess::Read, PolicyDecision::Ask);
        assert_eq!(store.room_policy(Some("!private-room"), RoomAccess::Read), PolicyDecision::Deny);
        assert_eq!(store.room_policy_blockers(Some("!private-room"), RoomAccess::Read).len(), 1);
        store.set_space_policy("!secret-two", RoomAccess::Read, PolicyDecision::Ask);
        assert_eq!(store.room_policy(Some("!private-room"), RoomAccess::Read), PolicyDecision::Allow);
        assert!(store.room_policy_blockers(Some("!private-room"), RoomAccess::Read).is_empty());
        store.set_global_policy(RoomAccess::Read, PolicyDecision::Deny);
        store.set_room_policy("!private-room", RoomAccess::Read, PolicyDecision::Deny);
        assert_eq!(store.room_policy_blockers(Some("!private-room"), RoomAccess::Read),
            vec![RoomPolicyReason::GlobalBlock, RoomPolicyReason::RoomRule { room: "!private-room" }]);
    }

    #[test]
    fn whitelist_collections_admit_only_filterable_allowed_targets() {
        let mut store = PermissionStore::default();
        let cap = crate::capabilities::by_id("matrix.rooms.list").unwrap();
        store.set_policy_mode(RoomAccess::Read, RoomPolicyMode::WhitelistOnly);
        store.set("app", Permission::MatrixRoomsList, GrantState::Granted);
        assert_eq!(store.effective_collection_capability_for_in_context("app", |_| true, |_| true, cap, PermissionContext::default()), Effective::Denied);
        store.set_room_policy("!selected", RoomAccess::Read, PolicyDecision::Allow);
        assert_eq!(store.effective_collection_capability_for_in_context("app", |_| true, |_| true, cap, PermissionContext::default()), Effective::Granted);
        assert_eq!(effective(&store, cap.id, context("!origin", "!selected")), Effective::Granted);
        assert_eq!(effective(&store, cap.id, context("!origin", "!outside")), Effective::Denied);
        store.set("app", Permission::MatrixRoomsList, GrantState::Denied);
        assert_eq!(store.effective_collection_capability_for_in_context("app", |_| true, |_| true, cap, PermissionContext::default()), Effective::Denied);
    }

    #[test]
    fn unknown_hierarchy_fails_closed_until_resolved() {
        let mut store = PermissionStore::default();
        store.set_global_policy(RoomAccess::Read, PolicyDecision::Allow);
        store.set_space_policy("!secret", RoomAccess::Read, PolicyDecision::Deny);
        assert_eq!(store.room_policy(Some("!unknown"), RoomAccess::Read), PolicyDecision::Deny);
        store.set_room_spaces("!unknown", vec![]);
        assert_eq!(store.room_policy(Some("!unknown"), RoomAccess::Read), PolicyDecision::Allow);
        store.clear_room_spaces();
        assert_eq!(store.room_policy(Some("!unknown"), RoomAccess::Read), PolicyDecision::Deny);
    }

    #[test]
    fn target_policy_overrides_origin_and_individual_grants() {
        let mut store = PermissionStore::default();
        store.set("app", Permission::MatrixRoomsRead, GrantState::Granted);
        store.set_room_policy("!secret", RoomAccess::Read, PolicyDecision::Deny);
        assert_eq!(effective(&store, "matrix.rooms.messages.read", context("!test", "!secret")), Effective::Denied);
        store.set_room_policy("!test", RoomAccess::Read, PolicyDecision::Allow);
        assert_eq!(effective(&store, "matrix.room.messages.read", context("!test", "!test")), Effective::Granted);
        store.set_capability("app", "matrix.room.messages.read", GrantState::Denied);
        assert_eq!(effective(&store, "matrix.room.messages.read", context("!test", "!test")), Effective::Denied);
        let cap = crate::capabilities::by_id("matrix.room.messages.read").unwrap();
        assert_eq!(store.effective_capability_for_in_context("app", |_| false, |_| true, cap, context("!test", "!test")), Effective::Undeclared);
    }

    #[test]
    fn scoped_capability_grants_do_not_leak_across_subjects_rooms_or_capabilities() {
        let mut store = PermissionStore::default();
        store.grant_scoped("app", Permission::MatrixRoomRead, Some("matrix.room.messages.read"),
            RoomScope::room("!one"), GrantDuration::Always, None).unwrap();
        assert_eq!(effective(&store, "matrix.room.messages.read", context("!two", "!one")), Effective::Granted);
        assert_eq!(effective(&store, "matrix.room.messages.read", context("!one", "!two")), Effective::NeedsPrompt);
        assert_eq!(effective(&store, "matrix.room.members.read", context("!one", "!one")), Effective::NeedsPrompt);
        assert!(!store.has_scoped_grant("other", Permission::MatrixRoomRead, Some("matrix.room.messages.read"), context("!one", "!one")));
        store.set("app", Permission::MatrixRoomRead, GrantState::Denied);
        assert_eq!(effective(&store, "matrix.room.messages.read", context("!one", "!one")), Effective::Denied);
    }

    #[test]
    fn narrowed_normal_permission_never_restores_global_auto_grant() {
        let mut store = PermissionStore::default();
        let cap = "matrix.room.info.read";
        assert_eq!(effective(&store, cap, context("!one", "!two")), Effective::Granted);
        store.grant_scoped("app", Permission::MatrixRoomInfo, None, RoomScope::room("!one"), GrantDuration::RoomSession, Some("!one")).unwrap();
        assert_eq!(effective(&store, cap, context("!one", "!one")), Effective::Granted);
        assert_eq!(effective(&store, cap, context("!one", "!two")), Effective::NeedsPrompt);
        store.clear_room_session("!one");
        assert_eq!(effective(&store, cap, context("!one", "!one")), Effective::NeedsPrompt);
        let mut restored: PermissionStore = serde_json::from_str(&serde_json::to_string(&store).unwrap()).unwrap();
        assert_eq!(effective(&restored, cap, context("!one", "!two")), Effective::NeedsPrompt);
        restored.set("app", Permission::MatrixRoomInfo, GrantState::Granted);
        assert_eq!(effective(&restored, cap, context("!one", "!two")), Effective::Granted);
    }

    #[test]
    fn space_subset_grants_follow_trusted_ancestors_and_empty_selection_is_not_global() {
        let mut store = PermissionStore::default();
        let selection = RoomScope::Selection { rooms: vec!["!one".into()], spaces: vec!["!space".into()] };
        store.grant_scoped("app", Permission::MatrixRoomsRead, None, selection, GrantDuration::Always, None).unwrap();
        assert_eq!(effective(&store, "matrix.rooms.messages.read", context("!origin", "!child")), Effective::NeedsPrompt);
        store.set_room_spaces("!child", vec!["!space".into()]);
        assert_eq!(effective(&store, "matrix.rooms.messages.read", context("!origin", "!child")), Effective::Granted);
        assert!(store.grant_scoped("app", Permission::Location, None, RoomScope::Selection { rooms: vec![], spaces: vec![] }, GrantDuration::Always, None).is_err());
    }

    #[test]
    fn room_and_robrix_sessions_survive_isolate_restart_but_not_their_lifetime() {
        let mut store = PermissionStore::default();
        store.grant_scoped("app", Permission::Location, None, RoomScope::AllRooms, GrantDuration::RoomSession, Some("!origin")).unwrap();
        store.grant_scoped("app", Permission::Camera, None, RoomScope::room("!one"), GrantDuration::RobrixSession, None).unwrap();
        store.grant_scoped("app", Permission::Microphone, None, RoomScope::room("!one"), GrantDuration::Always, None).unwrap();
        store.clear_once_for("app");
        assert!(store.has_scoped_grant("app", Permission::Location, None, context("!origin", "!one")));
        assert!(!store.has_scoped_grant("app", Permission::Location, None, context("!other", "!one")));
        let restored: PermissionStore = serde_json::from_str(&serde_json::to_string(&store).unwrap()).unwrap();
        assert!(!restored.has_scoped_grant("app", Permission::Location, None, context("!origin", "!one")));
        assert!(!restored.has_scoped_grant("app", Permission::Camera, None, context("!origin", "!one")));
        assert!(restored.has_scoped_grant("app", Permission::Microphone, None, context("!origin", "!one")));
        assert!(!store.clear_room_session("!other"));
        assert!(store.clear_room_session("!origin"));
        assert!(!store.has_scoped_grant("app", Permission::Location, None, context("!origin", "!one")));
        assert!(store.has_scoped_grant("app", Permission::Camera, None, context("!origin", "!one")));
    }

    #[test]
    fn migration_preserves_legacy_switch_without_repeating_v1_read_reset() {
        for enabled in [false, true] {
            let input = serde_json::json!({"schema":1,"grants":{"ai-room:!agent":{"matrix-rooms-read":"Granted"}},"matrix_write":enabled});
            let mut store: PermissionStore = serde_json::from_value(input).unwrap();
            store.migrate();
            assert_eq!(store.global_policy(RoomAccess::Read), PolicyDecision::Ask);
            assert_eq!(store.global_policy(RoomAccess::Write), if enabled { PolicyDecision::Ask } else { PolicyDecision::Deny });
            assert_eq!(store.state("ai-room:!agent", Permission::MatrixRoomsRead), GrantState::Granted);
            let mut roundtrip: PermissionStore = serde_json::from_str(&serde_json::to_string(&store).unwrap()).unwrap();
            roundtrip.migrate();
            assert_eq!(roundtrip.matrix_write(), enabled);
        }
    }

    #[test]
    fn network_scope_respects_url_origin_host_and_dns_boundaries() {
        let url = "https://Example.com:443/path?q=1#fragment";
        let exact = NetworkScope::from_url(url, NetworkScopeKind::ExactUrl).unwrap();
        assert!(exact.matches_url("https://example.com/path?q=1#different"));
        assert!(!exact.matches_url("https://example.com/path?q=2"));
        let origin = NetworkScope::from_url(url, NetworkScopeKind::Origin).unwrap();
        assert!(origin.matches_url("https://example.com/other"));
        assert!(!origin.matches_url("http://example.com/other"));
        assert!(!origin.matches_url("https://example.com:444/other"));
        let host = NetworkScope::from_url(url, NetworkScopeKind::Host).unwrap();
        assert!(host.matches_url("http://example.com:8000/other"));
        assert!(!host.matches_url("https://docs.example.com/"));
        let domain = NetworkScope::from_url(url, NetworkScopeKind::Domain).unwrap();
        assert!(domain.matches_url("https://docs.example.com/"));
        for spoof in ["https://notexample.com/", "https://example.com.attacker.test/", "https://example.com@attacker.test/", "https://attacker.test/?url=example.com", "file:///example.com"] {
            assert!(!domain.matches_url(spoof), "{spoof}");
        }
    }

    #[test]
    fn network_normalizes_idna_ipv6_and_trailing_dot_without_accepting_credentials() {
        let scope = NetworkScope::Host("bücher.example.".into()).normalized().unwrap();
        assert!(scope.matches_url("https://xn--bcher-kva.example./"));
        let v6 = NetworkScope::Host("[::1]".into()).normalized().unwrap();
        assert!(v6.matches_url("http://[::1]:8000/"));
        assert!(!v6.matches_url("http://[::2]/"));
        assert!(NetworkScope::Origin("https://example.com/path".into()).normalized().is_err());
        assert!(NetworkScope::Host("example.com:80".into()).normalized().is_err());
        assert!(!NetworkScope::AllHosts.matches_url("https://user:pass@example.com/"));
        let ip = NetworkScope::Domain("127.0.0.1".into());
        assert!(ip.matches_url("http://127.0.0.1/"));
        assert!(!ip.matches_url("http://sub.127.0.0.1/"));
        assert!(!ip.matches_url("http://127.0.0.1.example.com/"));
    }

    #[test]
    fn network_grants_use_room_lifetime_and_explicit_denials() {
        let mut store = PermissionStore::default();
        store.allow_network("app", NetworkScope::Host("example.com".into()), RoomScope::room("!one"), GrantDuration::RoomSession, Some("!one")).unwrap();
        assert!(store.is_url_allowed("app", "https://example.com/", context("!one", "!one")));
        assert!(!store.is_url_allowed("app", "https://example.com/", context("!two", "!one")));
        assert!(!store.is_url_allowed("app", "https://example.com/", context("!one", "!two")));
        store.clear_once_for("app");
        assert!(store.is_url_allowed("app", "https://example.com/", context("!one", "!one")));
        store.clear_room_session("!one");
        assert!(!store.is_url_allowed("app", "https://example.com/", context("!one", "!one")));
        store.allow_host("app", "example.com");
        assert!(store.is_url_allowed("app", "https://docs.example.com/", context("!one", "!one")), "legacy domains keep their meaning");
        store.set("app", Permission::Network, GrantState::Denied);
        assert!(!store.is_url_allowed("app", "https://example.com/", context("!one", "!one")));
    }

    #[test]
    fn network_once_receipt_is_exact_and_never_survives_serialization() {
        let mut store = PermissionStore::default();
        let id = store.allow_network("app", NetworkScope::ExactUrl("https://example.com/path".into()),
            RoomScope::room("!room"), GrantDuration::RobrixSession, None).unwrap();
        assert!(!store.has_network_request_once("app", "https://example.com/path", context("!room", "!room")));
        store.mark_network_request_once(id);
        let proof = store.clone();
        store.remove_network_grant(id);
        assert!(!store.has_network_request_once("app", "https://example.com/path", context("!room", "!room")));
        assert!(proof.has_network_request_once("app", "https://example.com/path", context("!room", "!room")));
        assert!(!proof.has_network_request_once("app", "https://example.com/other", context("!room", "!room")));
        assert!(!proof.has_network_request_once("other", "https://example.com/path", context("!room", "!room")));
        assert!(!proof.has_network_request_once("app", "https://example.com/path", context("!other", "!other")));
        let restored: PermissionStore = serde_json::from_str(&serde_json::to_string(&proof).unwrap()).unwrap();
        assert!(!restored.has_network_request_once("app", "https://example.com/path", context("!room", "!room")));
    }

    #[test]
    fn scoped_tools_stay_bound_to_description_and_exact_tool() {
        let mut store = PermissionStore::default();
        store.grant_scoped("app", Permission::McpTools, None, RoomScope::AllRooms, GrantDuration::Always, None).unwrap();
        assert!(!store.has_scoped_tool_grant("app", "tool", "hash", context("!one", "!one")));
        store.grant_scoped_tool("app", "tool", "hash", RoomScope::room("!one"), GrantDuration::Always, None).unwrap();
        assert!(store.has_scoped_tool_grant("app", "tool", "hash", context("!one", "!one")));
        assert!(!store.has_scoped_tool_grant("app", "tool", "new", context("!one", "!one")));
        assert!(!store.has_scoped_tool_grant("app", "other", "hash", context("!one", "!one")));
        store.deny_tool("app", "tool");
        assert!(!store.has_scoped_tool_grant("app", "tool", "hash", context("!one", "!one")));
    }

    #[test]
    fn revoked_grant_ids_are_not_reused_and_reset_keeps_protected_rooms() {
        let mut store = PermissionStore::default();
        let first = store.grant_scoped("app", Permission::Camera, None, RoomScope::AllRooms, GrantDuration::Always, None).unwrap();
        assert!(store.remove_scoped_grant(first));
        let second = store.grant_scoped("app", Permission::Camera, None, RoomScope::AllRooms, GrantDuration::Always, None).unwrap();
        assert!(second > first);
        assert!(!store.remove_scoped_grant(first));
        store.set_room_policy("!secret", RoomAccess::Read, PolicyDecision::Deny);
        store.reset_all();
        assert!(store.scoped_grants("app").is_empty());
        assert_eq!(store.room_policy(Some("!secret"), RoomAccess::Read), PolicyDecision::Deny);
    }

    #[test]
    fn request_once_proof_is_explicit_scoped_and_never_persisted() {
        let mut store = PermissionStore::default();
        let id = store.grant_scoped("app", Permission::MatrixRoomRead, None, RoomScope::room("!one"), GrantDuration::RobrixSession, None).unwrap();
        assert!(!store.has_request_once("app", "matrix.room.messages.read", context("!one", "!one")));
        store.mark_request_once(id);
        let proof = store.clone();
        store.remove_scoped_grant(id);
        assert!(!store.has_request_once("app", "matrix.room.messages.read", context("!one", "!one")));
        assert!(proof.has_request_once("app", "matrix.room.messages.read", context("!one", "!one")));
        assert!(!proof.has_request_once("app", "matrix.room.messages.read", context("!one", "!two")));
        assert!(!proof.has_request_once("other", "matrix.room.messages.read", context("!one", "!one")));
        assert!(!proof.has_request_once("app", "matrix.room.message.send", context("!one", "!one")));
        let restored: PermissionStore = serde_json::from_str(&serde_json::to_string(&proof).unwrap()).unwrap();
        assert!(!restored.has_request_once("app", "matrix.room.messages.read", context("!one", "!one")));
    }

    #[test]
    fn explicit_answers_clear_old_timed_grants_before_scoped_consent() {
        let mut store = PermissionStore::default();
        for answer in [GrantState::Ask, GrantState::Denied] {
            store.grant_until("app", Permission::Location, u64::MAX);
            store.set("app", Permission::Location, answer);
            assert!(store.timed_until("app", Permission::Location, 0).is_none());
            assert_eq!(store.effective_for("app", |_| true, Permission::Location), if answer == GrantState::Ask { Effective::NeedsPrompt } else { Effective::Denied });
        }
        store.set("app", Permission::Location, GrantState::Ask);
        store.grant_scoped("app", Permission::Location, None, RoomScope::room("!one"), GrantDuration::Always, None).unwrap();
        assert_eq!(store.effective_for_in_context("app", |_| true, Permission::Location, context("!two", "!two")), Effective::NeedsPrompt);
    }

    #[test]
    fn collection_consent_admits_subsets_but_only_matching_rows_are_readable() {
        let mut store = PermissionStore::default();
        let cap = crate::capabilities::by_id("matrix.rooms.list").unwrap();
        store.set_space_policy("!secret", RoomAccess::Read, PolicyDecision::Deny);
        store.grant_scoped("app", Permission::MatrixRoomsList, None, RoomScope::room("!one"), GrantDuration::Always, None).unwrap();
        assert_eq!(store.effective_collection_capability_for_in_context("app", |_| true, |_| true, cap, PermissionContext::default()), Effective::Granted);
        store.set_room_spaces("!one", vec![]);
        store.set_room_spaces("!two", vec![]);
        assert_eq!(effective(&store, cap.id, context("!origin", "!one")), Effective::Granted);
        assert_eq!(effective(&store, cap.id, context("!origin", "!two")), Effective::NeedsPrompt);
        store.set_room_spaces("!one", vec!["!secret".into()]);
        assert_eq!(effective(&store, cap.id, context("!origin", "!one")), Effective::Denied);
        store.set_global_policy(RoomAccess::Read, PolicyDecision::Deny);
        assert_eq!(store.effective_collection_capability_for_in_context("app", |_| true, |_| true, cap, PermissionContext::default()), Effective::Denied);
    }

    #[test]
    fn collection_evaluation_never_loosens_writes_or_room_session_ownership() {
        let mut store = PermissionStore::default();
        store.set_global_policy(RoomAccess::Write, PolicyDecision::Ask);
        let list = crate::capabilities::by_id("matrix.rooms.list").unwrap();
        let send = crate::capabilities::by_id("matrix.rooms.message.send").unwrap();
        store.grant_scoped("app", Permission::MatrixRoomsList, None, RoomScope::room("!one"), GrantDuration::RoomSession, Some("!origin")).unwrap();
        store.grant_scoped("app", Permission::MatrixRoomsSend, None, RoomScope::room("!one"), GrantDuration::Always, None).unwrap();
        assert_eq!(store.effective_collection_capability_for_in_context("app", |_| true, |_| true, list, context("!other", "!one")), Effective::NeedsPrompt);
        assert_eq!(store.effective_collection_capability_for_in_context("app", |_| true, |_| true, send, context("!origin", "!two")), Effective::NeedsPrompt);
    }

    #[test]
    fn configured_spaces_remain_resolution_roots_without_joined_membership() {
        let mut store = PermissionStore::default();
        store.set_space_policy("!left-space:s", RoomAccess::Read, PolicyDecision::Deny);
        store.grant_scoped("app", Permission::MatrixRoomsRead, None,
            RoomScope::Selection { rooms: Vec::new(), spaces: vec!["!grant-space:s".into()] },
            GrantDuration::Always, None).unwrap();
        store.allow_network("app", NetworkScope::AllHosts,
            RoomScope::Selection { rooms: Vec::new(), spaces: vec!["!session-space:s".into()] },
            GrantDuration::RoomSession, Some("!origin:s")).unwrap();
        assert_eq!(store.configured_space_ids(), BTreeSet::from([
            "!left-space:s".into(), "!grant-space:s".into(), "!session-space:s".into(),
        ]));
        store.clear_room_session("!origin:s");
        assert_eq!(store.configured_space_ids(), BTreeSet::from(["!left-space:s".into(), "!grant-space:s".into()]));
    }

}
