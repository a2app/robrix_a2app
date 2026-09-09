//! Services on the instance's attached room: info, messages, members, pins, threads.

use matrix_sdk::deserialized_responses::TimelineEvent;
use matrix_sdk::ruma::events::receipt::{ReceiptThread, ReceiptType};
use matrix_sdk::ruma::events::room::message::sanitize::remove_plain_reply_fallback;
use matrix_sdk::ruma::events::room::message::{OriginalSyncRoomMessageEvent, Relation};
use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent};
use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId, OwnedUserId};
use std::collections::HashMap;

use crate::sliding_sync::{current_user_id, get_client};
use super::clip_chars;

/// The event as an `m.room.message`, or None for state, reactions, redactions and the like.
fn as_message(event: &TimelineEvent) -> Option<OriginalSyncRoomMessageEvent> {
    match event.raw().deserialize() {
        Ok(AnySyncTimelineEvent::MessageLike(
            AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(msg))
        )) => Some(msg),
        _ => None,
    }
}

/// The `{sender, sender_id, event_id, body, ts, msgtype}` shape every message list carries.
fn message_json(msg: &OriginalSyncRoomMessageEvent) -> serde_json::Value {
    let mut body = remove_plain_reply_fallback(msg.content.body()).to_string();
    clip_chars(&mut body, 500);
    serde_json::json!({
        "sender": msg.sender.localpart(),
        "sender_id": msg.sender,
        "event_id": msg.event_id,
        "body": body,
        "ts": u64::from(msg.origin_server_ts.0),
        "msgtype": msg.content.msgtype(),
    })
}

/// The timestamp of the user's own newest read receipt (public or private)
/// in this room, if they have ever read it. Everything at or before that
/// point counts as read; everything after it, unread.
async fn my_read_receipt_ts(room: &matrix_sdk::Room) -> Option<u64> {
    let me = current_user_id()?;
    let candidates = [
        room.load_user_receipt(ReceiptType::Read, ReceiptThread::Unthreaded, &me).await.ok().flatten(),
        room.load_user_receipt(ReceiptType::ReadPrivate, ReceiptThread::Unthreaded, &me).await.ok().flatten(),
    ];
    candidates.into_iter().flatten().max_by_key(|(_, r)| r.ts)
        .and_then(|(_, r)| r.ts)
        .map(|t| u64::from(t.0))
}

/// Whether `msg` is still unread by the user: someone else sent it, and it
/// is newer than the user's own read receipt. The user's own messages are
/// always read (they wrote them).
fn is_unread(msg: &OriginalSyncRoomMessageEvent, my_read_ts: Option<u64>, me: &OwnedUserId) -> bool {
    msg.sender != *me
        && my_read_ts.is_none_or(|read_ts| u64::from(msg.origin_server_ts.0) > read_ts)
}

/// Adds the read-model context shared by the room-read rows to one row:
/// which room the message is in (so a caller can build a permalink to it,
/// including across rooms) and whether the user has already read it.
fn add_row_context(
    mut row: serde_json::Value,
    msg: &OriginalSyncRoomMessageEvent,
    room_id: &OwnedRoomId,
    my_read_ts: Option<u64>,
    me: &OwnedUserId,
) -> serde_json::Value {
    row["room_id"] = room_id.to_string().into();
    row["unread"] = is_unread(msg, my_read_ts, me).into();
    row
}

pub(super) async fn thread_replies(room_id: OwnedRoomId, event_id: OwnedEventId, limit: u32) -> Result<String, String> {
    use matrix_sdk::ruma::events::relation::RelationType;
    let client = get_client().ok_or("not logged in")?;
    let room = client.get_room(&room_id).ok_or("room not found")?;
    // Cache and store first; only a thread Robrix hasn't seen asks the homeserver.
    let (root, related) = room
        .load_or_fetch_event_with_relations(&event_id, Some(vec![RelationType::Thread]), None)
        .await
        .map_err(|e| format!("couldn't load the thread: {e}"))?;
    // The two sources order relations differently, so sort by time ourselves.
    let mut replies: Vec<OriginalSyncRoomMessageEvent> = related.iter().filter_map(as_message).collect();
    replies.sort_by_key(|m| m.origin_server_ts);
    let newest = replies.len().saturating_sub(limit as usize);
    let replies: Vec<serde_json::Value> = replies[newest..].iter().map(message_json).collect();
    Ok(serde_json::json!({
        "root": as_message(&root).map(|m| message_json(&m)),
        "replies": replies,
    }).to_string())
}

pub(crate) async fn older_messages(room_id: OwnedRoomId, before: Option<OwnedEventId>, limit: u32) -> Result<String, String> {
    use matrix_sdk::room::MessagesOptions;
    let client = get_client().ok_or("not logged in")?;
    let room = client.get_room(&room_id).ok_or("room not found")?;
    let limit = limit as usize;
    let me = current_user_id().ok_or("not logged in")?;
    let my_read_ts = my_read_receipt_ts(&room).await;
    // No anchor means "older than the cached window", i.e. where read_messages stops.
    let mut anchor = before;
    if anchor.is_none()
        && let Ok((cache, _guard)) = client.event_cache().room(&room_id).await
        && let Ok(events) = cache.events().await
    {
        anchor = events.first().and_then(|e| e.event_id()).map(ToOwned::to_owned);
    }
    let mut out: Vec<serde_json::Value> = Vec::new();
    let mut from: Option<String> = None;
    if let Some(anchor) = &anchor {
        // /context splits its budget across both sides of the anchor.
        let context = room.event_with_context(anchor, true, (limit as u32 * 2).into(), None).await
            .map_err(|e| format!("couldn't load older messages: {e}"))?;
        out.extend(context.events_before.iter().filter_map(as_message).map(|m| {
            add_row_context(message_json(&m), &m, &room_id, my_read_ts, &me)
        }));
        from = context.prev_batch_token;
    }
    // Top up from /messages when the page was mostly state events, or had no anchor.
    for _ in 0..4 {
        if out.len() >= limit || (anchor.is_some() && from.is_none()) {
            break;
        }
        let mut options = MessagesOptions::backward();
        options.limit = 50u32.into();
        options.from = from;
        let messages = room.messages(options).await
            .map_err(|e| format!("couldn't load older messages: {e}"))?;
        out.extend(messages.chunk.iter().filter_map(as_message).map(|m| {
            add_row_context(message_json(&m), &m, &room_id, my_read_ts, &me)
        }));
        from = messages.end;
        if from.is_none() {
            break;
        }
    }
    let has_more = from.is_some() || out.len() > limit;
    out.truncate(limit);
    // Both endpoints answer newest-first; apps read oldest-first.
    out.reverse();
    Ok(serde_json::json!({ "messages": out, "has_more": has_more }).to_string())
}

pub(super) async fn event(room_id: OwnedRoomId, event_id: OwnedEventId) -> Result<String, String> {
    use matrix_sdk::ruma::events::relation::RelationType;
    let client = get_client().ok_or("not logged in")?;
    let room = client.get_room(&room_id).ok_or("room not found")?;
    let me = current_user_id().ok_or("not logged in")?;
    let filter = Some(vec![RelationType::Annotation, RelationType::Replacement]);
    let (event, related) = room.load_or_fetch_event_with_relations(&event_id, filter, None).await
        .map_err(|e| format!("couldn't load the event: {e}"))?;
    let msg = as_message(&event).ok_or("that event isn't a message")?;
    // Start from the server's bundled edit; a later synced edit wins on time.
    let mut latest_edit = msg.unsigned.relations.replace.as_deref().cloned();
    let mut reactions: Vec<(String, u64, bool)> = Vec::new();
    for rel in &related {
        match rel.raw().deserialize() {
            Ok(AnySyncTimelineEvent::MessageLike(
                AnySyncMessageLikeEvent::Reaction(SyncMessageLikeEvent::Original(reaction))
            )) => {
                let key = reaction.content.relates_to.key;
                let mine = reaction.sender == me;
                match reactions.iter_mut().find(|(k, ..)| *k == key) {
                    Some((_, count, by_me)) => {
                        *count += 1;
                        *by_me |= mine;
                    }
                    None => reactions.push((key, 1, mine)),
                }
            }
            Ok(AnySyncTimelineEvent::MessageLike(
                AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(edit))
            )) if edit.sender == msg.sender
                && matches!(edit.content.relates_to, Some(Relation::Replacement(_)))
                && latest_edit.as_ref().is_none_or(|e| e.origin_server_ts < edit.origin_server_ts) =>
            {
                latest_edit = Some(edit);
            }
            _ => {}
        }
    }
    let mut out = message_json(&msg);
    if let Some(Relation::Replacement(edit)) = latest_edit.as_ref().and_then(|e| e.content.relates_to.as_ref()) {
        let mut body = remove_plain_reply_fallback(edit.new_content.msgtype.body()).to_string();
        clip_chars(&mut body, 500);
        out["body"] = body.into();
    }
    out["edited"] = latest_edit.is_some().into();
    out["reactions"] = reactions.into_iter()
        .map(|(key, count, mine)| serde_json::json!({ "key": key, "count": count, "mine": mine }))
        .collect();
    out["thread_root"] = match &msg.content.relates_to {
        Some(Relation::Thread(thread)) => serde_json::json!(thread.event_id),
        _ => serde_json::Value::Null,
    };
    Ok(out.to_string())
}

pub(super) async fn read_receipts(room_id: OwnedRoomId, user_id: Option<OwnedUserId>) -> Result<String, String> {
    use matrix_sdk::RoomMemberships;
    use matrix_sdk::ruma::events::receipt::{ReceiptThread, ReceiptType};
    // The user's "show read receipts" switch hides everyone's position, apps included.
    if !crate::settings::app_preferences::show_read_receipts() {
        return Ok(serde_json::json!({ "receipts": [] }).to_string());
    }
    let client = get_client().ok_or("not logged in")?;
    let room = client.get_room(&room_id).ok_or("room not found")?;
    let members = match user_id {
        Some(user_id) => {
            let member = room.get_member_no_sync(&user_id).await
                .map_err(|e| format!("couldn't look up that member: {e}"))?
                .ok_or("that user isn't in this room")?;
            vec![member]
        }
        None => room.members_no_sync(RoomMemberships::JOIN).await
            .map_err(|e| format!("couldn't load members: {e}"))?,
    };
    let me = current_user_id();
    let mut out: Vec<(u64, serde_json::Value)> = Vec::new();
    for member in members.iter().take(200) {
        let user_id = member.user_id();
        let mut candidates = vec![
            room.load_user_receipt(ReceiptType::Read, ReceiptThread::Unthreaded, user_id).await.ok().flatten(),
        ];
        // Our own position may only exist as a private receipt.
        if me.as_deref() == Some(user_id) {
            candidates.push(
                room.load_user_receipt(ReceiptType::ReadPrivate, ReceiptThread::Unthreaded, user_id).await.ok().flatten(),
            );
        }
        let Some((event_id, receipt)) = candidates.into_iter().flatten().max_by_key(|(_, r)| r.ts) else {
            continue;
        };
        let ts = receipt.ts.map(|t| u64::from(t.0));
        out.push((ts.unwrap_or(0), serde_json::json!({
            "user_id": user_id,
            "name": member.name(),
            "event_id": event_id,
            "ts": ts,
        })));
    }
    out.sort_by_key(|(ts, _)| std::cmp::Reverse(*ts));
    let receipts: Vec<serde_json::Value> = out.into_iter().map(|(_, v)| v).collect();
    Ok(serde_json::json!({ "receipts": receipts }).to_string())
}

pub(super) async fn unread(room_id: OwnedRoomId) -> Result<String, String> {
    let client = get_client().ok_or("not logged in")?;
    let room = client.get_room(&room_id).ok_or("room not found")?;
    Ok(serde_json::json!({
        "unread": room.num_unread_messages(),
        "mentions": room.num_unread_mentions(),
        "marked_unread": room.is_marked_unread(),
    }).to_string())
}

pub(super) async fn power_levels(room_id: OwnedRoomId) -> Result<String, String> {
    use matrix_sdk::ruma::events::room::power_levels::UserPowerLevel;
    use matrix_sdk::ruma::events::{MessageLikeEventType, StateEventType};
    let client = get_client().ok_or("not logged in")?;
    let room = client.get_room(&room_id).ok_or("room not found")?;
    let me = current_user_id().ok_or("not logged in")?;
    let levels = room.power_levels().await
        .map_err(|e| format!("couldn't load power levels: {e}"))?;
    // A room creator's power is "infinite" from room v12 on.
    let mine: i64 = match levels.for_user(&me) {
        UserPowerLevel::Int(int) => int.into(),
        _ => i64::MAX,
    };
    Ok(serde_json::json!({
        "mine": mine,
        "can": {
            "invite": levels.user_can_invite(&me),
            "kick": levels.user_can_kick(&me),
            "ban": levels.user_can_ban(&me),
            "redact_others": levels.user_can_redact_event_of_other(&me),
            "pin": levels.user_can_send_state(&me, StateEventType::RoomPinnedEvents),
            "send_message": levels.user_can_send_message(&me, MessageLikeEventType::RoomMessage),
            "notify_room": levels.user_can_trigger_room_notification(&me),
            "change_settings": levels.user_can_send_state(&me, StateEventType::RoomName)
                && levels.user_can_send_state(&me, StateEventType::RoomTopic),
        },
    }).to_string())
}

pub(super) async fn permalink(room_id: OwnedRoomId, event_id: Option<OwnedEventId>, use_matrix_scheme: bool) -> Result<String, String> {
    let client = get_client().ok_or("not logged in")?;
    let room = client.get_room(&room_id).ok_or("room not found")?;
    let url = match (use_matrix_scheme, event_id) {
        (true, Some(event_id)) => room.matrix_event_permalink(event_id).await.map(|u| u.to_string()),
        (true, None) => room.matrix_permalink(false).await.map(|u| u.to_string()),
        (false, Some(event_id)) => room.matrix_to_event_permalink(event_id).await.map(|u| u.to_string()),
        (false, None) => room.matrix_to_permalink().await.map(|u| u.to_string()),
    }.map_err(|e| format!("couldn't build the link: {e}"))?;
    Ok(serde_json::json!({ "url": url }).to_string())
}

pub(super) async fn successor(room_id: OwnedRoomId) -> Result<String, String> {
    let client = get_client().ok_or("not logged in")?;
    let room = client.get_room(&room_id).ok_or("room not found")?;
    let Some(successor) = room.successor_room() else {
        return Ok(serde_json::json!({
            "upgraded": false, "room_id": null, "name": null, "reason": null,
        }).to_string());
    };
    let name = match client.get_room(&successor.room_id) {
        Some(next) => match next.cached_display_name() {
            Some(name) => Some(name.to_string()),
            None => next.display_name().await.ok().map(|n| n.to_string()),
        },
        None => None,
    };
    Ok(serde_json::json!({
        "upgraded": true,
        "room_id": successor.room_id,
        "name": name,
        "reason": successor.reason,
    }).to_string())

}
/// `matrix.room_info`, and `matrix.rooms_info` for any joined room.
pub(crate) async fn info(room_id: matrix_sdk::ruma::OwnedRoomId) -> Result<String, String> {
    use matrix_sdk::RoomState;
    use crate::sliding_sync::get_client;
    let client = get_client().ok_or("not logged in")?;
    let room = client.get_room(&room_id).ok_or("room not found")?;
    if room.state() != RoomState::Joined {
        return Err("room not joined".into());
    }
    let room_name = room.display_name().await
        .map(|n| n.to_string())
        .unwrap_or_else(|_| room_id.to_string());
    let join_rule = room.join_rule()
        .map(|r| r.as_str().to_string())
        .unwrap_or_else(|| String::from("unknown"));
    let history = room.history_visibility_or_default().as_str().to_string();
    let body = serde_json::json!({
        "room_id": room_id.to_string(),
        "room_name": room_name,
        "topic": room.topic().unwrap_or_default(),
        "member_count": room.active_members_count(),
        "encrypted": room.encryption_state().is_encrypted(),
        "join_rule": join_rule,
        "history_visibility": history,
        "alias": room.canonical_alias().map(|a| a.to_string()),
    });
    Ok(body.to_string())
}

/// One row of `read_messages`. The walk is newest-first, so an edit shows up
/// before the message it replaces: keep its body and hand it to the original.
fn push_message(
    out: &mut Vec<serde_json::Value>,
    edits: &mut HashMap<OwnedEventId, String>,
    msg: OriginalSyncRoomMessageEvent,
    room_id: &OwnedRoomId,
    my_read_ts: Option<u64>,
    me: &OwnedUserId,
) {
    if let Some(Relation::Replacement(edit)) = &msg.content.relates_to {
        edits.entry(edit.event_id.clone())
            .or_insert_with(|| remove_plain_reply_fallback(edit.new_content.msgtype.body()).to_string());
        return;
    }
    let mut row = message_json(&msg);
    if let Some(mut body) = edits.remove(&msg.event_id) {
        clip_chars(&mut body, 500);
        row["body"] = body.into();
    }
    out.push(add_row_context(row, &msg, room_id, my_read_ts, me));
}

/// `matrix.read_messages`, and `matrix.rooms_messages` for any joined room.
pub(crate) async fn read_messages(room_id: matrix_sdk::ruma::OwnedRoomId, limit: u32) -> Result<String, String> {
    use matrix_sdk::RoomState;
    use matrix_sdk::room::MessagesOptions;
    use crate::sliding_sync::get_client;
    let client = get_client().ok_or("not logged in")?;
    let room = client.get_room(&room_id).ok_or("room not found")?;
    if room.state() != RoomState::Joined {
        return Err("room not joined".into());
    }
    // The user's own read receipt marks what they have already seen, so each
    // row can say whether it is unread — and the agent can focus on the
    // messages the user hasn't read yet when asked to summarize.
    let me = current_user_id().ok_or("not logged in")?;
    let my_read_ts = my_read_receipt_ts(&room).await;
    let mut out: Vec<serde_json::Value> = Vec::new();
    let mut edits = HashMap::new();
    // The event cache already holds the recent timeline in
    // memory; only hit the network when it can't fill the request.
    if let Ok((cache, _guard)) = client.event_cache().room(&room_id).await {
        if let Ok(events) = cache.events().await {
            for event in events.iter().rev() {
                let Ok(AnySyncTimelineEvent::MessageLike(
                    AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(msg))
                )) = event.raw().deserialize() else { continue };
                push_message(&mut out, &mut edits, msg, &room_id, my_read_ts, &me);
                if out.len() >= limit as usize {
                    break;
                }
            }
        }
    }
    if out.len() < limit as usize {
        // A room's recent tail can be all state events (profile
        // changes etc), so keep paginating until we fill `limit`.
        out.clear();
        edits.clear();
        let mut from: Option<String> = None;
        for _ in 0..4 {
            let mut options = MessagesOptions::backward();
            options.limit = 50u32.into();
            options.from = from;
            let messages = room.messages(options).await
                .map_err(|e| format!("couldn't read messages: {e}"))?;
            for event in messages.chunk {
                let Ok(AnySyncTimelineEvent::MessageLike(
                    AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(msg))
                )) = event.raw().deserialize() else { continue };
                push_message(&mut out, &mut edits, msg, &room_id, my_read_ts, &me);
                if out.len() >= limit as usize {
                    break;
                }
            }
            from = messages.end;
            if out.len() >= limit as usize || from.is_none() {
                break;
            }
        }
    }
    // Backward pagination is newest-first; apps read oldest-first.
    out.reverse();
    Ok(serde_json::json!({ "messages": out, "unread_count": room.num_unread_messages() }).to_string())
}
