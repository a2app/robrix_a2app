//! Services across many rooms: the room list and search.

use matrix_sdk::{Room, RoomState};
use matrix_sdk::ruma::{OwnedRoomOrAliasId, OwnedServerName};
use matrix_sdk::ruma::room::RoomType;

use crate::sliding_sync::get_client;

/// The cached name when there is one; computing it can read the member store.
pub(super) async fn room_name(room: &Room) -> String {
    match room.cached_display_name() {
        Some(name) => name.to_string(),
        None => room.display_name().await
            .map(|n| n.to_string())
            .unwrap_or_else(|_| room.room_id().to_string()),
    }
}

pub(super) async fn search(query: String, limit: u32) -> Result<String, String> {
    let client = get_client().ok_or("not logged in")?;
    let needle = query.to_lowercase();
    let mut out: Vec<serde_json::Value> = Vec::new();
    for room in client.joined_rooms().into_iter().chain(client.invited_rooms()) {
        let name = room_name(&room).await;
        let alias_hit = room.canonical_alias()
            .is_some_and(|a| a.as_str().to_lowercase().contains(&needle));
        if !name.to_lowercase().contains(&needle) && !alias_hit {
            continue;
        }
        out.push(serde_json::json!({
            "room_id": room.room_id(),
            "name": name,
            "is_direct": room.is_direct().await.unwrap_or(false),
            "is_space": room.is_space(),
            "member_count": room.joined_members_count(),
            "is_encrypted": room.encryption_state().is_encrypted(),
            "unread": room.num_unread_messages(),
            "mentions": room.num_unread_mentions(),
            "joined": room.state() == RoomState::Joined,
        }));
        if out.len() >= limit as usize {
            break;
        }
    }
    Ok(serde_json::json!({ "rooms": out }).to_string())
}

pub(super) async fn invites() -> Result<String, String> {
    let client = get_client().ok_or("not logged in")?;
    let mut out: Vec<serde_json::Value> = Vec::new();
    for room in client.invited_rooms() {
        let invite = room.invite_details().await.ok();
        let inviter_name = invite.as_ref().map(|i| match &i.inviter {
            Some(member) => member.name().to_string(),
            None => i.inviter_id.localpart().to_string(),
        });
        out.push(serde_json::json!({
            "room_id": room.room_id(),
            "name": room_name(&room).await,
            "is_space": room.is_space(),
            "is_direct": room.is_direct().await.unwrap_or(false),
            "inviter_id": invite.as_ref().map(|i| &i.inviter_id),
            "inviter_name": inviter_name,
        }));
    }
    Ok(serde_json::json!({ "invites": out }).to_string())
}

pub(super) async fn preview(room: OwnedRoomOrAliasId, via: Vec<OwnedServerName>) -> Result<String, String> {
    let client = get_client().ok_or("not logged in")?;
    let preview = client.get_room_preview(&room, via).await
        .map_err(|e| format!("couldn't preview the room: {e}"))?;
    let name = preview.name.clone()
        .or_else(|| preview.canonical_alias.as_ref().map(|a| a.to_string()))
        .unwrap_or_else(|| preview.room_id.to_string());
    Ok(serde_json::json!({
        "room_id": preview.room_id,
        "name": name,
        "topic": preview.topic.unwrap_or_default(),
        "member_count": preview.num_joined_members,
        "join_rule": preview.join_rule.as_ref().map(|r| r.as_str()).unwrap_or("unknown"),
        "is_space": matches!(preview.room_type, Some(RoomType::Space)),
        "joined": preview.state == Some(RoomState::Joined),
    }).to_string())
}
