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

pub(crate) async fn list() -> Result<String, String> {
    let client = get_client().ok_or("not logged in")?;
    let mut out: Vec<serde_json::Value> = Vec::new();
    for space in client.joined_rooms().into_iter().filter(|r| r.is_space()) {
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
    let client = get_client().ok_or("not logged in")?;
    let space = client.get_room(&space_id).ok_or("space not found")?;
    if !space.is_space() || space.state() != RoomState::Joined {
        return Err("not a joined space".into());
    }
    // Counted the way the SDK's space graph does: every child state event
    // that still deserializes, redactions excluded.
    let children_count = space.get_state_events_static::<SpaceChildEventContent>().await
        .map(|children| children.iter()
            .filter(|c| matches!(c.deserialize(),
                Ok(SyncOrStrippedState::Sync(SyncStateEvent::Original(_)) | SyncOrStrippedState::Stripped(_))))
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
    let client = get_client().ok_or("not logged in")?;
    let list = SpaceRoomList::new(client, space_id).await;
    // Each page is one /hierarchy request; stop at the end or at the row cap.
    loop {
        list.paginate().await.map_err(|e| format!("couldn't load the space's rooms: {e}"))?;
        let done = matches!(list.pagination_state(), SpaceRoomListPaginationState::Idle { end_reached: true });
        if done || list.rooms().await.len() >= 200 {
            break;
        }
    }
    let out: Vec<serde_json::Value> = list.rooms().await.into_iter()
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
    Ok(serde_json::json!({ "rooms": out }).to_string())
}
