//! Writes into the attached room: messages, replies, reactions, typing,
//! receipts and the room's own flags.

use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;

use a2app_core::services::matrix::RoomFlag;

use crate::home::rooms_list::{enqueue_rooms_list_update, RoomsListUpdate};
use crate::sliding_sync::get_client;

/// The transport retains both compound permissions through upload and send.
/// Every field binds the permissions to the same activation and exact media.
pub(super) fn validate_media_authorizations(
    room_id: &OwnedRoomId,
    primary: &super::MatrixAuthorization,
    upload: &super::MatrixAuthorization,
    payload: &serde_json::Value,
) -> Result<(), String> {
    if primary.capability != "matrix.media.send" || upload.capability != "matrix.media.upload"
        || primary.subject != upload.subject || primary.subject.is_empty()
        || primary.flow_context.is_none() || primary.flow_context != upload.flow_context
        || primary.flow_epoch.is_none_or(|epoch| epoch == 0) || primary.flow_epoch != upload.flow_epoch
        || !matches!(primary.flow_context.as_ref(), Some(a2app_core::information_flow::ContextId::App { app, room: Some(origin), .. })
            if app == &primary.subject && origin == room_id.as_str())
        || primary.origin_room.as_deref() != Some(room_id.as_str())
        || primary.target_room.as_deref() != Some(room_id.as_str())
        || upload.origin_room != primary.origin_room || upload.target_room != primary.target_room
        || primary.flow_payload.as_ref() != Some(payload) || upload.flow_payload.as_ref() != Some(payload)
    {
        return Err("Media-send and upload permission must cover this attachment, room, and app activation.".into());
    }
    Ok(())
}

pub(super) async fn media(
    room_id: OwnedRoomId,
    media: std::sync::Arc<crate::a2app::ai::media::PreparedMedia>,
    primary: Option<&super::MatrixAuthorization>,
    upload: Option<&super::MatrixAuthorization>,
) -> Result<String, String> {
    let primary = primary.ok_or("Missing media-send permission.")?;
    let upload = upload.ok_or("Missing media-upload permission.")?;
    validate_media_authorizations(&room_id, primary, upload, &media.post_payload(&room_id))?;
    media.post(&room_id, &[primary.clone(), upload.clone()]).await
}

pub(super) async fn message(room_id: OwnedRoomId, body: String) -> Result<String, String> {
    super::policy::ensure_room_access(room_id.as_str(), a2app_core::permissions::RoomAccess::Write)?;
    let client = get_client().ok_or("not logged in")?;
    let room = client.get_room(&room_id).ok_or("room not found")?;
    let content = RoomMessageEventContent::text_plain(body);
    super::policy::commit_sensitive_target(room_id.as_str(), &serde_json::to_value(&content).map_err(|_| "Cannot review message content.")?).await?;
    super::policy::audit_room_operation(room_id.as_str(), room.send(content)).await
        .map_err(|e| format!("couldn't send the message: {e}"))?;
    Ok(String::from("{}"))
}

pub(super) async fn reply(
    room_id: OwnedRoomId,
    event_id: OwnedEventId,
    body: String,
    in_thread: bool,
) -> Result<String, String> {
    use matrix_sdk::room::reply::{EnforceThread, Reply};
    use matrix_sdk::ruma::events::room::message::{AddMentions, ReplyWithinThread, RoomMessageEventContentWithoutRelation};
    super::policy::ensure_room_access(room_id.as_str(), a2app_core::permissions::RoomAccess::Write)?;
    let client = get_client().ok_or("not logged in")?;
    let room = client.get_room(&room_id).ok_or("room not found")?;
    super::policy::ensure_server_output(client.homeserver().as_str()).await?;
    // A thread post isn't a reply to the root, so it gets no mention; a plain
    // reply mentions its target the way the composer does.
    let reply = if in_thread {
        Reply { event_id, enforce_thread: EnforceThread::Threaded(ReplyWithinThread::No), add_mentions: AddMentions::No }
    } else {
        Reply { event_id, enforce_thread: EnforceThread::MaybeThreaded, add_mentions: AddMentions::Yes }
    };
    let content = super::policy::audit_server_operation(client.homeserver().as_str(), room.make_reply_event(RoomMessageEventContentWithoutRelation::text_plain(body), reply)).await
        .map_err(|e| format!("couldn't build the reply: {e}"))?;
    super::policy::ensure_room_access(room_id.as_str(), a2app_core::permissions::RoomAccess::Write)?;
    super::policy::ensure_server_output(client.homeserver().as_str()).await?;
    super::policy::commit_sensitive_target(room_id.as_str(), &serde_json::to_value(&content).map_err(|_| "Cannot review reply content.")?).await?;
    let sent = super::policy::audit_room_operation(room_id.as_str(), room.send(content)).await
        .map_err(|e| format!("couldn't send the reply: {e}"))?;
    Ok(serde_json::json!({ "event_id": sent.response.event_id }).to_string())
}

pub(super) async fn react(room_id: OwnedRoomId, event_id: OwnedEventId, key: String) -> Result<String, String> {
    use matrix_sdk::room::{IncludeRelations, RelationsOptions};
    use matrix_sdk::ruma::events::reaction::ReactionEventContent;
    use matrix_sdk::ruma::events::relation::{Annotation, RelationType};
    use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent};
    super::policy::ensure_room_access(room_id.as_str(), a2app_core::permissions::RoomAccess::Write)?;
    let client = get_client().ok_or("not logged in")?;
    let room = client.get_room(&room_id).ok_or("room not found")?;
    super::policy::ensure_server_output(client.homeserver().as_str()).await?;
    // Page through the event's reactions until we find our own with this key.
    let mut from = None;
    let mine = loop {
        let opts = RelationsOptions {
            from,
            include_relations: IncludeRelations::RelationsOfType(RelationType::Annotation),
            ..Default::default()
        };
        super::policy::ensure_server_output(client.homeserver().as_str()).await?;
        let page = super::policy::audit_server_operation(client.homeserver().as_str(), room.relations(event_id.clone(), opts)).await
            .map_err(|e| format!("couldn't load the reactions: {e}"))?;
        let found = page.chunk.iter().find_map(|event| {
            let Ok(AnySyncTimelineEvent::MessageLike(
                AnySyncMessageLikeEvent::Reaction(SyncMessageLikeEvent::Original(reaction))
            )) = event.raw().deserialize() else { return None };
            (&*reaction.sender == room.own_user_id() && reaction.content.relates_to.key == key)
                .then_some(reaction.event_id)
        });
        if found.is_some() || page.prev_batch_token.is_none() {
            break found;
        }
        from = page.prev_batch_token;
    };
    super::policy::ensure_room_access(room_id.as_str(), a2app_core::permissions::RoomAccess::Write)?;
    super::policy::ensure_server_output(client.homeserver().as_str()).await?;
    let added = match mine {
        Some(reaction_id) => {
            super::policy::commit_sensitive_target(room_id.as_str(), &serde_json::json!({ "remove_reaction": reaction_id, "event_id": event_id, "key": key })).await?;
            super::policy::audit_server_operation(client.homeserver().as_str(), room.redact(&reaction_id, None, None)).await
                .map_err(|e| format!("couldn't remove the reaction: {e}"))?;
            false
        }
        None => {
            let content = ReactionEventContent::new(Annotation::new(event_id, key));
            super::policy::commit_sensitive_target(room_id.as_str(), &serde_json::to_value(&content).map_err(|_| "Cannot review reaction content.")?).await?;
            super::policy::audit_room_operation(room_id.as_str(), room.send(content)).await
                .map_err(|e| format!("couldn't send the reaction: {e}"))?;
            true
        }
    };
    Ok(serde_json::json!({ "added": added }).to_string())
}

pub(super) async fn typing(room_id: OwnedRoomId, typing: bool) -> Result<String, String> {
    // The user's "let others see when you're typing" switch applies to apps too.
    if typing && !crate::settings::app_preferences::send_typing_notices() {
        return Ok(String::from("{}"));
    }
    super::policy::ensure_room_access(room_id.as_str(), a2app_core::permissions::RoomAccess::Write)?;
    let client = get_client().ok_or("not logged in")?;
    let room = client.get_room(&room_id).ok_or("room not found")?;
    super::policy::commit_sensitive_target(room_id.as_str(), &serde_json::json!({ "typing": typing })).await?;
    super::policy::audit_server_operation(client.homeserver().as_str(), room.typing_notice(typing)).await
        .map_err(|e| format!("couldn't send the typing notice: {e}"))?;
    Ok(String::from("{}"))
}

pub(super) async fn read_receipt(room_id: OwnedRoomId, event_id: Option<OwnedEventId>) -> Result<String, String> {
    use matrix_sdk::room::Receipts;
    use matrix_sdk::ruma::api::client::receipt::create_receipt::v3::ReceiptType;
    use matrix_sdk::ruma::events::receipt::ReceiptThread;
    use matrix_sdk_base::latest_event::LatestEventValue;
    use crate::settings::app_preferences::preferred_receipt_type;
    super::policy::ensure_room_access(room_id.as_str(), a2app_core::permissions::RoomAccess::Write)?;
    let client = get_client().ok_or("not logged in")?;
    let room = client.get_room(&room_id).ok_or("room not found")?;
    super::policy::ensure_server_output(client.homeserver().as_str()).await?;
    let receipt_type = preferred_receipt_type();
    if let Some(event_id) = event_id {
        super::policy::commit_sensitive_target(room_id.as_str(), &serde_json::json!({ "event_id": event_id, "type": receipt_type, "thread": "unthreaded" })).await?;
        super::policy::audit_server_operation(client.homeserver().as_str(), room.send_single_receipt(receipt_type, ReceiptThread::Unthreaded, event_id)).await
            .map_err(|e| format!("couldn't send the read receipt: {e}"))?;
        return Ok(String::from("{}"));
    }
    // Fully read means up to the newest event we know of: the SDK's
    // latest-event slot first, else the tail of the event cache.
    let mut latest = match room.latest_event() {
        LatestEventValue::Remote(event) => event.event_id().map(ToOwned::to_owned),
        _ => None,
    };
    if latest.is_none()
        && let Ok((cache, _guard)) = client.event_cache().room(&room_id).await
        && let Ok(events) = cache.events().await
    {
        latest = events.iter().rev().find_map(|e| e.event_id().map(ToOwned::to_owned));
    }
    let latest = latest.ok_or("no messages to mark as read")?;
    let payload = serde_json::json!({ "fully_read": latest, "receipt_event_id": latest, "receipt_type": receipt_type });
    let receipts = Receipts::new().fully_read_marker(latest.clone());
    let receipts = if matches!(receipt_type, ReceiptType::ReadPrivate) {
        receipts.private_read_receipt(latest)
    } else {
        receipts.public_read_receipt(latest)
    };
    super::policy::ensure_room_access(room_id.as_str(), a2app_core::permissions::RoomAccess::Write)?;
    super::policy::ensure_server_output(client.homeserver().as_str()).await?;
    super::policy::commit_sensitive_target(room_id.as_str(), &payload).await?;
    super::policy::audit_server_operation(client.homeserver().as_str(), room.send_multiple_receipts(receipts)).await
        .map_err(|e| format!("couldn't mark the room as read: {e}"))?;
    Ok(String::from("{}"))
}

pub(super) async fn pin(room_id: OwnedRoomId, event_id: OwnedEventId, pinned: bool) -> Result<String, String> {
    super::policy::ensure_room_access(room_id.as_str(), a2app_core::permissions::RoomAccess::Write)?;
    let client = get_client().ok_or("not logged in")?;
    let room = client.get_room(&room_id).ok_or("room not found")?;
    super::policy::commit_sensitive_target(room_id.as_str(), &serde_json::json!({ "event_id": event_id, "pinned": pinned })).await?;
    let result = if pinned {
        super::policy::audit_server_operation(client.homeserver().as_str(), room.pin_event(&event_id)).await
    } else {
        super::policy::audit_server_operation(client.homeserver().as_str(), room.unpin_event(&event_id)).await
    };
    result.map_err(|e| format!("couldn't {} the message: {e}", if pinned { "pin" } else { "unpin" }))?;
    Ok(String::from("{}"))
}

pub(super) async fn room_flag(room_id: OwnedRoomId, flag: RoomFlag, on: bool) -> Result<String, String> {
    super::policy::ensure_room_access(room_id.as_str(), a2app_core::permissions::RoomAccess::Write)?;
    let client = get_client().ok_or("not logged in")?;
    let room = client.get_room(&room_id).ok_or("room not found")?;
    let flag_name = match flag { RoomFlag::Favorite => "favorite", RoomFlag::LowPriority => "low_priority", RoomFlag::Unread => "unread" };
    super::policy::commit_sensitive_target(room_id.as_str(), &serde_json::json!({ "flag": flag_name, "on": on })).await?;
    let result = match flag {
        RoomFlag::Favorite => super::policy::audit_server_operation(client.homeserver().as_str(), room.set_is_favourite(on, None)).await,
        RoomFlag::LowPriority => super::policy::audit_server_operation(client.homeserver().as_str(), room.set_is_low_priority(on, None)).await,
        RoomFlag::Unread => super::policy::audit_server_operation(client.homeserver().as_str(), room.set_unread_flag(on)).await,
    };
    result.map_err(|e| format!("couldn't update the room flag: {e}"))?;
    if matches!(flag, RoomFlag::Unread) {
        enqueue_rooms_list_update(RoomsListUpdate::UpdateMarkedUnread { room_id, is_marked_unread: on });
    }
    Ok(String::from("{}"))
}

#[cfg(test)]
mod media_tests {
    use super::*;

    #[tokio::test]
    async fn media_post_without_both_permissions_refuses_before_any_matrix_client_access() {
        let room: OwnedRoomId = "!media:test".try_into().unwrap();
        let request = crate::a2app::ai::tools::MediaDraftRequest {
            source: crate::a2app::ai::tools::MediaSource::Base64("ZmlsZQ==".into()),
            filename: "fixture.txt".into(), mime_type: "text/plain".into(), caption: None,
        };
        let draft = std::sync::Arc::new(crate::a2app::ai::media::PreparedMedia::from_base64(&request).unwrap());
        assert!(media(room.clone(), draft.clone(), None, None).await.unwrap_err().contains("media-send permission"));
        let primary = super::super::MatrixAuthorization::new("media-app", "matrix.media.send", Some(room.as_str()),
            &a2app_core::permissions::PermissionStore::default());
        assert!(media(room, draft, Some(&primary), None).await.unwrap_err().contains("media-upload permission"));
    }

    #[test]
    fn media_permissions_are_bound_to_both_capabilities_and_the_exact_activation_room_and_bytes() {
        let room: OwnedRoomId = "!media:test".try_into().unwrap();
        let payload = serde_json::json!({"room_id":room,"media":{"sha256":"reviewed bytes"}});
        let mut primary = super::super::MatrixAuthorization::new("media-app", "matrix.media.send", Some(room.as_str()),
            &a2app_core::permissions::PermissionStore::default());
        primary.flow_context = Some(a2app_core::information_flow::ContextId::App {
            account: "@media:test".into(), app: "media-app".into(), room: Some(room.to_string()),
        });
        primary.flow_epoch = Some(1);
        primary.flow_payload = Some(payload.clone());
        let mut upload = primary.clone();
        upload.capability = "matrix.media.upload".into();
        assert!(validate_media_authorizations(&room, &primary, &upload, &payload).is_ok());
        for field in ["capability", "subject", "context", "epoch", "origin", "target", "payload"] {
            let mut changed = upload.clone();
            match field {
                "capability" => changed.capability = "matrix.room.message.send".into(),
                "subject" => changed.subject = "different-app".into(),
                "context" => changed.flow_context = None,
                "epoch" => changed.flow_epoch = Some(2),
                "origin" => changed.origin_room = Some("!different:test".into()),
                "target" => changed.target_room = Some("!different:test".into()),
                "payload" => changed.flow_payload = Some(serde_json::json!({"room_id":room,"media":{"sha256":"changed bytes"}})),
                _ => unreachable!(),
            }
            assert!(validate_media_authorizations(&room, &primary, &changed, &payload).is_err(), "accepted changed {field}");
        }
        let mut detached = primary.clone();
        detached.flow_context = Some(a2app_core::information_flow::ContextId::App {
            account: "@media:test".into(), app: "media-app".into(), room: None,
        });
        let mut detached_upload = detached.clone();
        detached_upload.capability = upload.capability;
        assert!(validate_media_authorizations(&room, &detached, &detached_upload, &payload).is_err());
    }
}
