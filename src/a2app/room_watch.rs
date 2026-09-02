//! Worker-side room watches: new messages, member-count and pin changes in a
//! watched room get posted to the UI thread as `A2AppRoomWatchEvent`s.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use makepad_widgets::{log, Cx, SignalToUI};
use eyeball_im::VectorDiff;
use matrix_sdk::RoomState;
use matrix_sdk::event_cache::{EventsOrigin, RoomEventCacheUpdate, TimelineVectorDiffs};
use matrix_sdk::ruma::{MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId};
use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncStateEvent, AnySyncTimelineEvent, SyncMessageLikeEvent};
use tokio::runtime::Handle;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

use crate::sliding_sync::{current_user_id, get_client};

/// What a watched room did, posted to the UI thread for hook delivery.
#[derive(Clone, Debug)]
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
    MembersChanged { count: u64 },
    PinsChanged { pinned: Vec<OwnedEventId> },
    /// The room's stream ended (left, tombstoned, or the cache closed).
    Closed,
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
        let watch_start = MilliSecondsSinceUnixEpoch::now();
        let own = current_user_id();
        let mut last_pins = room.pinned_event_ids().unwrap_or_default();
        let mut last_count = room.joined_members_count();
        let mut members_dirty = false;
        loop {
            tokio::select! {
                update = sub.recv() => {
                    match update {
                        Ok(RoomEventCacheUpdate::UpdateTimelineEvents(
                            TimelineVectorDiffs { diffs, origin: EventsOrigin::Sync }
                        )) => {
                            for diff in diffs {
                                let events: Vec<_> = match diff {
                                    VectorDiff::Append { values } => values.into_iter().collect(),
                                    VectorDiff::PushBack { value } => vec![value],
                                    _ => continue,
                                };
                                for ev in events {
                                    let Ok(event) = ev.raw().deserialize() else { continue };
                                    let msg = match event {
                                        AnySyncTimelineEvent::MessageLike(
                                            AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(msg))
                                        ) => msg,
                                        AnySyncTimelineEvent::State(AnySyncStateEvent::RoomMember(_)) => {
                                            members_dirty = true;
                                            continue;
                                        }
                                        _ => continue,
                                    };
                                    // Sync can re-deliver older events after a gap; only report what's new.
                                    if msg.origin_server_ts < watch_start { continue }
                                    let sender_name = room.get_member_no_sync(&msg.sender).await
                                        .ok().flatten()
                                        .and_then(|m| m.display_name().map(str::to_string))
                                        .unwrap_or_else(|| msg.sender.localpart().to_string());
                                    let mut body = msg.content.body().to_string();
                                    if let Some((cut, _)) = body.char_indices().nth(500) {
                                        body.truncate(cut);
                                    }
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
                            }
                        }
                        Ok(RoomEventCacheUpdate::UpdateMembers { .. }) => members_dirty = true,
                        Ok(_) | Err(RecvError::Lagged(_)) => {}
                        Err(RecvError::Closed) => return Ok(()),
                    }
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
                }
            }
        }
    }.await;
    if let Err(e) = ended {
        log!("Room watch for {room_id} ended: {e}");
    }
    post(RoomWatchKind::Closed);
}
