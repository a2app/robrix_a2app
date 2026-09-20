//! Services across many rooms: the room list and search.

use matrix_sdk::{Room, RoomState};
use matrix_sdk::ruma::{OwnedRoomId, OwnedRoomOrAliasId, OwnedServerName, RoomAliasId};
use matrix_sdk::ruma::room::RoomType;

use crate::sliding_sync::get_client;
use a2app_core::permissions::RoomAccess;
use super::policy::{ensure_room_access, room_access_allowed};

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
        if !room_access_allowed(room.room_id().as_str(), RoomAccess::Read) { continue }
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

/// Every joined room the model may offer to read, as JSON rows with the
/// name and id (plus direct/space/encryption/unread flags). The read tools'
/// `list_rooms` fetches this; the shape matches `matrix.rooms_list`.
pub(crate) async fn joined_rooms_list() -> Result<String, String> {
    let client = get_client().ok_or("not logged in")?;
    let mut out: Vec<serde_json::Value> = Vec::new();
    for room in client.joined_rooms() {
        if !room_access_allowed(room.room_id().as_str(), RoomAccess::Read) { continue }
        out.push(serde_json::json!({
            "room_id": room.room_id(),
            "name": room_name(&room).await,
            "is_direct": room.is_direct().await.unwrap_or(false),
            "is_space": room.is_space(),
            "member_count": room.joined_members_count(),
            "is_encrypted": room.encryption_state().is_encrypted(),
            "unread": room.num_unread_messages(),
            "mentions": room.num_unread_mentions(),
        }));
    }
    Ok(serde_json::json!({ "rooms": out }).to_string())
}

pub(super) async fn invites() -> Result<String, String> {
    let client = get_client().ok_or("not logged in")?;
    let mut out: Vec<serde_json::Value> = Vec::new();
    for room in client.invited_rooms() {
        if !room_access_allowed(room.room_id().as_str(), RoomAccess::Read) { continue }
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

pub(super) async fn preview(room: OwnedRoomOrAliasId, mut via: Vec<OwnedServerName>) -> Result<String, String> {
    let client = get_client().ok_or("not logged in")?;
    let target = resolve_room_id(&client, &room, &mut via).await?;
    ensure_room_access(target.as_str(), RoomAccess::Read)?;
    let room = OwnedRoomOrAliasId::from(target);
    super::policy::ensure_server_output(client.homeserver().as_str())?;
    let preview = super::policy::audit_server_operation(client.homeserver().as_str(), client.get_room_preview(&room, via)).await
        .map_err(|e| format!("couldn't preview the room: {e}"))?;
    ensure_room_access(preview.room_id.as_str(), RoomAccess::Read)?;
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

pub(super) async fn resolve_room_id(client: &matrix_sdk::Client, room: &OwnedRoomOrAliasId, via: &mut Vec<OwnedServerName>) -> Result<OwnedRoomId, String> {
    if room.as_str().starts_with('!') {
        return OwnedRoomId::try_from(room.as_str()).map_err(|_| "invalid room id".to_string());
    }
    let alias = <&RoomAliasId>::try_from(room.as_str()).map_err(|_| "invalid room alias")?;
    super::policy::ensure_server_output(client.homeserver().as_str())?;
    let resolved = super::policy::audit_server_operation(client.homeserver().as_str(), client.resolve_room_alias(alias)).await
        .map_err(|e| format!("couldn't resolve the room alias: {e}"))?;
    for server in resolved.servers {
        if !via.contains(&server) { via.push(server); }
    }
    Ok(resolved.room_id)
}
