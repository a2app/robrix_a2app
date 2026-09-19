//! Services on spaces: the joined spaces, one space's details, and its child rooms.

use matrix_sdk::RoomState;
use matrix_sdk::deserialized_responses::SyncOrStrippedState;
use matrix_sdk::ruma::OwnedRoomId;
use matrix_sdk::ruma::events::SyncStateEvent;
use matrix_sdk::ruma::events::room::history_visibility::HistoryVisibility;
use matrix_sdk::ruma::events::space::child::SpaceChildEventContent;
use matrix_sdk::ruma::room::RoomType;
use matrix_sdk_ui::spaces::SpaceRoomList;
use matrix_sdk_ui::spaces::room_list::SpaceRoomListPaginationState;

use super::rooms::room_name;
use crate::sliding_sync::get_client;
use a2app_core::permissions::RoomAccess;
use super::policy::{ensure_room_access, room_access_allowed};

/// A complete, host-only ancestry snapshot. Failures never mark a partial
/// graph complete: unknown membership stays blocked by space deny rules.
#[derive(Clone, Debug)]
pub struct A2AppPolicySpaces {
    pub rooms: Vec<(String, Vec<String>)>,
    pub error: Option<String>,
    pub revision: u64,
}

#[derive(Clone, Debug)]
pub struct A2AppPolicySpacesInvalidated;

static POLICY_SPACES_REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static POLICY_SPACE_WATCHES: std::sync::LazyLock<std::sync::Mutex<Vec<matrix_sdk::event_handler::EventHandlerDropGuard>>> = std::sync::LazyLock::new(Default::default);

pub fn policy_spaces_revision() -> u64 {
    POLICY_SPACES_REVISION.load(std::sync::atomic::Ordering::SeqCst)
}

pub fn stop_policy_space_watch() {
    POLICY_SPACE_WATCHES.lock().unwrap().clear();
    // Results from a previous login must not republish its membership graph.
    POLICY_SPACES_REVISION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    super::policy::invalidate_room_spaces();
}

pub(crate) fn invalidate_policy_spaces() {
    POLICY_SPACES_REVISION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    super::policy::invalidate_room_spaces();
    makepad_widgets::Cx::post_action(A2AppPolicySpacesInvalidated);
    makepad_widgets::SignalToUI::set_ui_signal();
}

fn watch_policy_spaces(client: &matrix_sdk::Client) {
    use matrix_sdk::ruma::events::space::{child::SyncSpaceChildEvent, parent::SyncSpaceParentEvent};
    use matrix_sdk::ruma::events::room::member::SyncRoomMemberEvent;
    let child = client.add_event_handler(|_: SyncSpaceChildEvent| async { invalidate_policy_spaces(); });
    let parent = client.add_event_handler(|_: SyncSpaceParentEvent| async { invalidate_policy_spaces(); });
    let own_user = client.user_id().map(ToOwned::to_owned);
    let membership = client.add_event_handler(move |event: SyncRoomMemberEvent| {
        let own_user = own_user.clone();
        async move {
            if own_user.as_ref().is_some_and(|user| event.state_key() == user) {
                invalidate_policy_spaces();
            }
        }
    });
    // Replace guards together, covering a changed login/client as well.
    *POLICY_SPACE_WATCHES.lock().unwrap() = vec![
        client.event_handler_drop_guard(child), client.event_handler_drop_guard(parent),
        client.event_handler_drop_guard(membership),
    ];
}

fn ancestors(
    rooms: impl IntoIterator<Item = String>,
    parents: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
) -> Vec<(String, Vec<String>)> {
    rooms.into_iter().map(|room| {
        let mut seen = std::collections::BTreeSet::new();
        let mut pending: Vec<String> = parents.get(&room).into_iter().flatten().cloned().collect();
        while let Some(parent) = pending.pop() {
            if parent != room && seen.insert(parent.clone()) {
                pending.extend(parents.get(&parent).into_iter().flatten().cloned());
            }
        }
        (room, seen.into_iter().collect())
    }).collect()
}

fn has_unresolved_branches(
    parents: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    returned: &std::collections::BTreeSet<String>,
    known_leaves: &std::collections::BTreeSet<String>,
) -> bool {
    // A missing, known ordinary room has no descendants. A missing space or
    // unknown room might hide a joined descendant; absence is not exclusion.
    parents.keys().any(|child| !returned.contains(child) && !known_leaves.contains(child))
}

/// The paginated hierarchy includes unopened spaces and nested subspaces;
/// visible UI room lists are insufficient to establish a negative membership.
pub async fn refresh_policy_spaces() {
    use std::collections::{BTreeMap, BTreeSet};
    use matrix_sdk::ruma::api::client::space::get_hierarchy;
    let revision = policy_spaces_revision();
    let result: Result<Vec<(String, Vec<String>)>, String> = async {
        let client = get_client().ok_or("not logged in")?;
        watch_policy_spaces(&client);
        let mut known: BTreeSet<String> = client.rooms().iter().map(|r| r.room_id().to_string()).collect();
        let known_leaves: BTreeSet<String> = client.rooms().iter().filter(|r| !r.is_space())
            .map(|r| r.room_id().to_string()).collect();
        let mut returned = BTreeSet::new();
        let mut parents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let joined_spaces: BTreeSet<OwnedRoomId> = client.joined_rooms().into_iter()
            .filter(|room| room.is_space()).map(|room| room.room_id().to_owned()).collect();
        let mut roots = BTreeSet::new();
        for id in super::policy::configured_space_ids() {
            let id = OwnedRoomId::try_from(id).map_err(|_| "A configured space has an invalid room ID.")?;
            if !joined_spaces.contains(&id) {
                // A one-time hierarchy fetch cannot maintain a protection
                // boundary when we no longer receive the space's sync events.
                return Err("A space with permission rules is no longer joined. Rejoin it or remove its space rule; unresolved protection remains blocked.".into());
            }
            roots.insert(id);
        }
        for space in roots {
            let mut from = None;
            let mut tokens = BTreeSet::new();
            loop {
                let mut request = get_hierarchy::v1::Request::new(space.clone());
                request.from = from;
                request.limit = Some(100u32.into());
                let response = client.send(request).await
                    .map_err(|e| format!("couldn't refresh room safety rules: {e}"))?;
                for entry in response.rooms {
                    if matches!(entry.summary.room_type, Some(RoomType::Space))
                        && !joined_spaces.contains(&entry.summary.room_id)
                    {
                        // Even a currently empty subspace can gain children.
                        // Its membership cannot stay verified without sync events.
                        return Err("A protected hierarchy contains an unjoined subspace. Join it to keep space protection current; unresolved protection remains blocked.".into());
                    }
                    let parent = entry.summary.room_id.to_string();
                    returned.insert(parent.clone());
                    known.insert(parent.clone());
                    for child in entry.children_state {
                        let Ok(child) = child.deserialize() else { continue };
                        if child.content.via.is_empty() { continue }
                        let child = child.state_key.to_string();
                        known.insert(child.clone());
                        parents.entry(child).or_default().insert(parent.clone());
                    }
                }
                from = response.next_batch;
                let Some(token) = from.as_ref() else { break };
                if !tokens.insert(token.clone()) || tokens.len() > 1000 {
                    return Err("space hierarchy pagination did not finish".to_string());
                }
            }
            if !returned.contains(space.as_str()) {
                return Err("The homeserver omitted a space while checking room safety rules. Access protected by space rules remains blocked.".to_string());
            }
        }
        if revision != policy_spaces_revision() {
            return Err("space memberships changed while safety rules were refreshing".to_string());
        }
        if has_unresolved_branches(&parents, &returned, &known_leaves) {
            return Err("Some space memberships are inaccessible. Access protected by space rules remains blocked until the hierarchy can be verified.".to_string());
        }
        Ok(ancestors(known, &parents))
    }.await;
    let (rooms, error) = match result {
        Ok(rooms) => (rooms, None),
        Err(error) => (Vec::new(), Some(error)),
    };
    makepad_widgets::Cx::post_action(A2AppPolicySpaces { rooms, error, revision });
    makepad_widgets::SignalToUI::set_ui_signal();
}

pub(crate) async fn list() -> Result<String, String> {
    let client = get_client().ok_or("not logged in")?;
    let mut out: Vec<serde_json::Value> = Vec::new();
    for space in client.joined_rooms().into_iter().filter(|r| r.is_space()) {
        if !room_access_allowed(space.room_id().as_str(), RoomAccess::Read) { continue }
        out.push(serde_json::json!({
            "space_id": space.room_id(),
            "name": room_name(&space).await,
            "topic": space.topic().unwrap_or_default(),
            "member_count": space.joined_members_count(),
        }));
    }
    Ok(serde_json::json!({ "spaces": out }).to_string())
}

pub(crate) async fn info(space_id: OwnedRoomId) -> Result<String, String> {
    ensure_room_access(space_id.as_str(), RoomAccess::Read)?;
    let client = get_client().ok_or("not logged in")?;
    let space = client.get_room(&space_id).ok_or("space not found")?;
    if !space.is_space() || space.state() != RoomState::Joined {
        return Err("not a joined space".into());
    }
    // Counted the way the SDK's space graph does: every child state event
    // that still deserializes, redactions excluded.
    let children_count = space.get_state_events_static::<SpaceChildEventContent>().await
        .map(|children| children.iter()
            .filter(|c| match c.deserialize() {
                Ok(SyncOrStrippedState::Sync(SyncStateEvent::Original(event))) =>
                    !event.content.via.is_empty() && room_access_allowed(event.state_key.as_str(), RoomAccess::Read),
                Ok(SyncOrStrippedState::Stripped(event)) =>
                    event.content.via.as_ref().is_some_and(|via| !via.is_empty()) && room_access_allowed(event.state_key.as_str(), RoomAccess::Read),
                _ => false,
            })
            .count())
        .unwrap_or(0);
    let join_rule = space.join_rule()
        .map(|r| r.as_str().to_string())
        .unwrap_or_else(|| String::from("unknown"));
    Ok(serde_json::json!({
        "space_id": space_id,
        "name": room_name(&space).await,
        "topic": space.topic().unwrap_or_default(),
        "member_count": space.joined_members_count(),
        "join_rule": join_rule,
        "world_readable": space.history_visibility_or_default() == HistoryVisibility::WorldReadable,
        "children_count": children_count,
    }).to_string())
}

pub(crate) async fn rooms(space_id: OwnedRoomId) -> Result<String, String> {
    let check_space = || {
        super::policy::global_room_access_allowed(space_id.as_str(), RoomAccess::Read)
            .then_some(()).ok_or_else(|| super::policy::ROOM_ACCESS_DENIED.to_string())
    };
    check_space()?;
    let client = get_client().ok_or("not logged in")?;
    let list = SpaceRoomList::new(client, space_id.clone()).await;
    // Each page is one /hierarchy request; stop at the end or at the row cap.
    loop {
        check_space()?;
        list.paginate().await.map_err(|e| format!("couldn't load the space's rooms: {e}"))?;
        let done = matches!(list.pagination_state(), SpaceRoomListPaginationState::Idle { end_reached: true });
        if done || list.rooms().await.len() >= 200 {
            break;
        }
    }
    let out: Vec<serde_json::Value> = list.rooms().await.into_iter()
        .filter(|room| room_access_allowed(room.room_id.as_str(), RoomAccess::Read))
        .take(200)
        .map(|r| serde_json::json!({
            "room_id": r.room_id,
            "name": r.display_name,
            "topic": r.topic.unwrap_or_default(),
            "is_space": matches!(r.room_type, Some(RoomType::Space)),
            "joined": r.state == Some(RoomState::Joined),
            "member_count": r.num_joined_members,
            "join_rule": r.join_rule.as_ref().map(|j| j.as_str()).unwrap_or("unknown"),
        }))
        .collect();
    check_space()?;
    Ok(serde_json::json!({ "rooms": out }).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ancestry_follows_nested_and_overlapping_spaces_without_cycle_loops() {
        let parents = [
            ("!room:s", vec!["!child:s", "!other:s"]),
            ("!child:s", vec!["!root:s"]),
            ("!root:s", vec!["!child:s"]),
        ].into_iter().map(|(room, parents)| (
            room.to_string(), parents.into_iter().map(str::to_string).collect(),
        )).collect();
        let result = ancestors(vec!["!room:s".to_string(), "!root:s".to_string(), "!alone:s".to_string()], &parents);
        assert_eq!(result[0].1, vec!["!child:s", "!other:s", "!root:s"]);
        assert_eq!(result[1].1, vec!["!child:s"]);
        assert!(result[2].1.is_empty());
    }

    #[test]
    fn missing_subspaces_cannot_turn_unknown_membership_into_an_empty_ancestor_set() {
        let parents = [("!missing:s".into(), ["!protected:s".into()].into_iter().collect())].into_iter().collect();
        let returned = ["!protected:s".into()].into_iter().collect();
        assert!(has_unresolved_branches(&parents, &returned, &Default::default()));
        let known_leaf = ["!missing:s".into()].into_iter().collect();
        assert!(!has_unresolved_branches(&parents, &returned, &known_leaf));
        let returned = ["!protected:s".into(), "!missing:s".into()].into_iter().collect();
        assert!(!has_unresolved_branches(&parents, &returned, &Default::default()));
    }
}
