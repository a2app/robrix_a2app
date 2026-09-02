//! Worker-side room watches: whatever a watched room does gets posted to
//! the UI thread as `A2AppRoomWatchEvent`s for hook delivery.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use makepad_widgets::{log, Cx, SignalToUI};
use eyeball_im::VectorDiff;
use matrix_sdk::{Room, RoomInfo, RoomState};
use matrix_sdk::event_cache::{EventsOrigin, RoomEventCacheUpdate, TimelineVectorDiffs};
use matrix_sdk::ruma::{MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, UserId};
use matrix_sdk::ruma::events::{AnySyncEphemeralRoomEvent, AnySyncMessageLikeEvent, AnySyncStateEvent, AnySyncTimelineEvent, SyncMessageLikeEvent};
use matrix_sdk::ruma::events::receipt::ReceiptType;
use matrix_sdk::ruma::events::room::message::Relation;
use tokio::runtime::Handle;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

use crate::a2app::matrix::clip_chars;
use crate::sliding_sync::{current_user_id, get_client};

/// What a watched room did, posted to the UI thread for hook delivery.
#[derive(Clone, Debug, PartialEq)]
pub enum RoomWatchKind {
    Message {
        event_id: OwnedEventId,
        sender: OwnedUserId,
        sender_name: String,
        body: String,
        msgtype: String,
        ts: u64,
        is_own: bool,
    },
    /// An edit (`body` is the new text) or a redaction of a message.
    MessageChanged { event_id: OwnedEventId, edited: bool, redacted: bool, body: String },
    /// A removed reaction only knows its target and key if this watch saw it added.
    Reaction { event_id: Option<OwnedEventId>, key: Option<String>, sender: OwnedUserId, added: bool },
    Typing { users: Vec<(OwnedUserId, String)> },
    Receipts { receipts: Vec<RoomReceipt> },
    MembersChanged { count: u64 },
    PinsChanged { pinned: Vec<OwnedEventId> },
    InfoChanged {
        name: String,
        topic: String,
        encrypted: bool,
        is_favorite: bool,
        is_low_priority: bool,
        upgraded: bool,
    },
    UnreadChanged { unread: u64, mentions: u64, marked_unread: bool },
    /// The room's stream ended (left, tombstoned, or the cache closed).
    Closed,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RoomReceipt {
    pub user_id: OwnedUserId,
    pub event_id: OwnedEventId,
    pub ts: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct A2AppRoomWatchEvent {
    pub room_id: OwnedRoomId,
    pub kind: RoomWatchKind,
}

static ROOM_WATCHES: LazyLock<Mutex<HashMap<OwnedRoomId, JoinHandle<()>>>> = LazyLock::new(Default::default);

/// Spawns a watch for `room_id` on the current tokio runtime, replacing any existing one.
pub fn start_watch(room_id: OwnedRoomId) {
    let task = Handle::current().spawn(watch_room(room_id.clone()));
    if let Some(old) = ROOM_WATCHES.lock().unwrap().insert(room_id, task) {
        old.abort();
    }
}

pub fn stop_watch(room_id: &RoomId) {
    if let Some(task) = ROOM_WATCHES.lock().unwrap().remove(room_id) {
        task.abort();
    }
}

pub fn stop_all() {
    for (_, task) in ROOM_WATCHES.lock().unwrap().drain() {
        task.abort();
    }
}

async fn member_name(room: &Room, user_id: &UserId) -> String {
    room.get_member_no_sync(user_id).await
        .ok().flatten()
        .and_then(|m| m.display_name().map(str::to_string))
        .unwrap_or_else(|| user_id.localpart().to_string())
}

/// Watches one room until it's left or its event cache closes; every exit posts `Closed`.
/// Runs on the matrix worker's tokio runtime, so `start_watch` is the normal entry point.
pub async fn watch_room(room_id: OwnedRoomId) {
    let post = |kind| {
        Cx::post_action(A2AppRoomWatchEvent { room_id: room_id.clone(), kind });
        SignalToUI::set_ui_signal();
    };
    let ended: Result<(), String> = async {
        let client = get_client().ok_or("not logged in")?;
        let room = client.get_room(&room_id).ok_or("room not found")?;
        let (cache, _guard) = client.event_cache().room(&room_id).await
            .map_err(|e| format!("event cache unavailable: {e}"))?;
        let (_initial, mut sub) = cache.subscribe().await
            .map_err(|e| format!("couldn't subscribe to the event cache: {e}"))?;
        let mut info = room.subscribe_info();
        let (_typing_guard, mut typing) = room.subscribe_to_typing_notifications();
        let watch_start = MilliSecondsSinceUnixEpoch::now();
        let own = current_user_id();
        let info_snapshot = |room_info: &RoomInfo| RoomWatchKind::InfoChanged {
            name: room.cached_display_name().map(|n| n.to_string()).unwrap_or_else(|| room_id.to_string()),
            topic: room_info.topic().unwrap_or_default().to_string(),
            encrypted: room_info.encryption_state().is_encrypted(),
            is_favorite: room.is_favourite(),
            is_low_priority: room.is_low_priority(),
            upgraded: room_info.tombstone().is_some(),
        };
        let unread_snapshot = |room_info: &RoomInfo| RoomWatchKind::UnreadChanged {
            unread: room_info.read_receipts().num_unread,
            mentions: room_info.read_receipts().num_mentions,
            marked_unread: room.is_marked_unread(),
        };
        let mut last_pins = room.pinned_event_ids().unwrap_or_default();
        let mut last_count = room.joined_members_count();
        let mut last_info = info_snapshot(&room.clone_info());
        let mut last_unread = unread_snapshot(&room.clone_info());
        let mut members_dirty = false;
        // Reactions this watch saw, by reaction event id, so a later redaction
        // can still say what it took away.
        let mut reactions: HashMap<OwnedEventId, (OwnedEventId, String)> = HashMap::new();
        loop {
            tokio::select! {
                update = sub.recv() => {
                    match update {
                        Ok(RoomEventCacheUpdate::UpdateTimelineEvents(
                            TimelineVectorDiffs { diffs, origin: EventsOrigin::Sync }
                        )) => {
                            for diff in diffs {
                                // Appends are new events; a Set is the cache redacting one it holds.
                                let (events, fresh): (Vec<_>, bool) = match diff {
                                    VectorDiff::Append { values } => (values.into_iter().collect(), true),
                                    VectorDiff::PushBack { value } => (vec![value], true),
                                    VectorDiff::Set { value, .. } => (vec![value], false),
                                    _ => continue,
                                };
                                for ev in events {
                                    let Ok(event) = ev.raw().deserialize() else { continue };
                                    match (event, fresh) {
                                        (AnySyncTimelineEvent::MessageLike(
                                            AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(msg))
                                        ), true) => {
                                            // Sync can re-deliver older events after a gap; only report what's new.
                                            if msg.origin_server_ts < watch_start { continue }
                                            if let Some(Relation::Replacement(edit)) = &msg.content.relates_to {
                                                let mut body = edit.new_content.msgtype.body().to_string();
                                                clip_chars(&mut body, 500);
                                                post(RoomWatchKind::MessageChanged {
                                                    event_id: edit.event_id.clone(),
                                                    edited: true,
                                                    redacted: false,
                                                    body,
                                                });
                                                continue;
                                            }
                                            let sender_name = member_name(&room, &msg.sender).await;
                                            let mut body = msg.content.body().to_string();
                                            clip_chars(&mut body, 500);
                                            let is_own = own.as_ref() == Some(&msg.sender);
                                            post(RoomWatchKind::Message {
                                                event_id: msg.event_id,
                                                sender: msg.sender,
                                                sender_name,
                                                body,
                                                msgtype: msg.content.msgtype().to_string(),
                                                ts: u64::from(msg.origin_server_ts.get()),
                                                is_own,
                                            });
                                        }
                                        (AnySyncTimelineEvent::MessageLike(
                                            AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Redacted(msg))
                                        ), false) => {
                                            post(RoomWatchKind::MessageChanged {
                                                event_id: msg.event_id,
                                                edited: false,
                                                redacted: true,
                                                body: String::new(),
                                            });
                                        }
                                        (AnySyncTimelineEvent::MessageLike(
                                            AnySyncMessageLikeEvent::Reaction(SyncMessageLikeEvent::Original(reaction))
                                        ), true) => {
                                            if reaction.origin_server_ts < watch_start { continue }
                                            let target = reaction.content.relates_to.event_id;
                                            let key = reaction.content.relates_to.key;
                                            reactions.insert(reaction.event_id, (target.clone(), key.clone()));
                                            post(RoomWatchKind::Reaction {
                                                event_id: Some(target),
                                                key: Some(key),
                                                sender: reaction.sender,
                                                added: true,
                                            });
                                        }
                                        (AnySyncTimelineEvent::MessageLike(
                                            AnySyncMessageLikeEvent::Reaction(SyncMessageLikeEvent::Redacted(reaction))
                                        ), false) => {
                                            let (event_id, key) = reactions.remove(&reaction.event_id).unzip();
                                            post(RoomWatchKind::Reaction {
                                                event_id,
                                                key,
                                                sender: reaction.sender,
                                                added: false,
                                            });
                                        }
                                        (AnySyncTimelineEvent::State(AnySyncStateEvent::RoomMember(_)), _) => {
                                            members_dirty = true;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                        Ok(RoomEventCacheUpdate::AddEphemeralEvents { events }) => {
                            let mut receipts = Vec::new();
                            for raw in events {
                                let Ok(AnySyncEphemeralRoomEvent::Receipt(receipt_event)) = raw.deserialize() else { continue };
                                for (event_id, by_type) in receipt_event.content.0 {
                                    for (kind, users) in by_type {
                                        if !matches!(kind, ReceiptType::Read | ReceiptType::ReadPrivate) { continue }
                                        receipts.extend(users.into_iter().map(|(user_id, receipt)| RoomReceipt {
                                            user_id,
                                            event_id: event_id.clone(),
                                            ts: receipt.ts.map(|ts| u64::from(ts.get())),
                                        }));
                                    }
                                }
                            }
                            if !receipts.is_empty() {
                                post(RoomWatchKind::Receipts { receipts });
                            }
                        }
                        Ok(RoomEventCacheUpdate::UpdateMembers { .. }) => members_dirty = true,
                        Ok(_) | Err(RecvError::Lagged(_)) => {}
                        Err(RecvError::Closed) => return Ok(()),
                    }
                }
                next = typing.recv() => {
                    let user_ids = match next {
                        Ok(user_ids) => user_ids,
                        Err(RecvError::Lagged(_)) => continue,
                        Err(RecvError::Closed) => return Ok(()),
                    };
                    let mut users = Vec::with_capacity(user_ids.len());
                    for user_id in user_ids {
                        let name = member_name(&room, &user_id).await;
                        users.push((user_id, name));
                    }
                    post(RoomWatchKind::Typing { users });
                }
                next = info.next() => {
                    let Some(room_info) = next else { return Ok(()) };
                    if room_info.state() != RoomState::Joined { return Ok(()) }
                    let pins = room_info.pinned_event_ids().unwrap_or_default();
                    if pins != last_pins {
                        last_pins = pins.clone();
                        post(RoomWatchKind::PinsChanged { pinned: pins });
                    }
                    let count = room_info.joined_members_count();
                    if members_dirty || count != last_count {
                        last_count = count;
                        members_dirty = false;
                        post(RoomWatchKind::MembersChanged { count });
                    }
                    let snapshot = info_snapshot(&room_info);
                    if snapshot != last_info {
                        last_info = snapshot.clone();
                        post(snapshot);
                    }
                    let snapshot = unread_snapshot(&room_info);
                    if snapshot != last_unread {
                        last_unread = snapshot.clone();
                        post(snapshot);
                    }
                }
            }
        }
    }.await;
    if let Err(e) = ended {
        log!("Room watch for {room_id} ended: {e}");
    }
    post(RoomWatchKind::Closed);
}
