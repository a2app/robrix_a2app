//! Membership changes an app can ask for: invites, joins, invite answers
//! and starting DMs.

use matrix_sdk::RoomState;
use matrix_sdk::ruma::{OwnedRoomId, OwnedRoomOrAliasId, OwnedServerName, OwnedUserId};

use crate::sliding_sync::get_client;
use a2app_core::permissions::RoomAccess;
use super::policy::ensure_room_access;

pub(super) async fn invite(room_id: OwnedRoomId, user_id: OwnedUserId) -> Result<String, String> {
    ensure_room_access(room_id.as_str(), RoomAccess::Write)?;
    let client = get_client().ok_or("not logged in")?;
    let room = client.get_room(&room_id).ok_or("room not found")?;
    super::policy::ensure_server_output(client.homeserver().as_str())?;
    room.invite_user_by_id(&user_id).await
        .map_err(|e| format!("couldn't invite {user_id}: {e}"))?;
    Ok(String::from("{}"))
}

pub(super) async fn join(room: OwnedRoomOrAliasId, mut via: Vec<OwnedServerName>) -> Result<String, String> {
    use matrix_sdk::ruma::api::error::ErrorKind;
    let client = get_client().ok_or("not logged in")?;
    let target = super::rooms::resolve_room_id(&client, &room, &mut via).await?;
    ensure_room_access(target.as_str(), RoomAccess::Write)?;
    // Resolve once, then use the checked id: an alias changing during the
    // request cannot redirect a permitted join into a protected room.
    let room = OwnedRoomOrAliasId::from(target);
    super::policy::ensure_server_output(client.homeserver().as_str())?;
    let (joined, knocked) = match client.join_room_by_id_or_alias(&room, &via).await {
        Ok(joined) => (joined, false),
        // An invite-only room refuses the join, so knocking is the next best ask.
        Err(e) if matches!(e.client_api_error_kind(), Some(ErrorKind::Forbidden)) => {
            ensure_room_access(room.as_str(), RoomAccess::Write)?;
            super::policy::ensure_server_output(client.homeserver().as_str())?;
            let knocked = client.knock(room, None, via).await
                .map_err(|e| format!("couldn't knock on the room: {e}"))?;
            (knocked, true)
        }
        Err(e) => return Err(format!("couldn't join the room: {e}")),
    };
    Ok(serde_json::json!({
        "room_id": joined.room_id(),
        "joined": !knocked,
        "knocked": knocked,
    }).to_string())
}

pub(super) async fn invite_respond(room_id: OwnedRoomId, accept: bool) -> Result<String, String> {
    ensure_room_access(room_id.as_str(), RoomAccess::Write)?;
    let client = get_client().ok_or("not logged in")?;
    let room = client.get_room(&room_id).ok_or("room not found")?;
    if room.state() != RoomState::Invited {
        return Err("there's no pending invite for that room".into());
    }
    super::policy::ensure_server_output(client.homeserver().as_str())?;
    let result = if accept { room.join().await } else { room.leave().await };
    result.map_err(|e| format!("couldn't {} the invite: {e}", if accept { "accept" } else { "decline" }))?;
    Ok(String::from("{}"))
}

pub(super) async fn dm_open(user_id: OwnedUserId) -> Result<String, String> {
    let client = get_client().ok_or("not logged in")?;
    let (room, created) = match client.get_dm_room(&user_id) {
        Some(room) => {
            ensure_room_access(room.room_id().as_str(), RoomAccess::Read)?;
            ensure_room_access(room.room_id().as_str(), RoomAccess::Write)?;
            (room, false)
        },
        None => {
            if !super::policy::global_room_access_allowed("", RoomAccess::Write) {
                return Err(super::policy::ROOM_ACCESS_DENIED.to_string());
            }
            super::policy::ensure_server_output(client.homeserver().as_str())?;
            super::policy::ensure_sensitive_target("host")?;
            let room = client.create_dm(&user_id).await
                .map_err(|e| format!("couldn't start a chat with {user_id}: {e}"))?;
            (room, true)
        }
    };
    Ok(serde_json::json!({ "room_id": room.room_id(), "created": created }).to_string())
}
