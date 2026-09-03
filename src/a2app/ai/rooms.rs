//! AI Rooms: the Matrix operations that back the marker/forwarding/reply
//! machinery (create a room, read/write its marker, read/write its
//! forwarding cursor, write an `ai_reply`). Run on the async worker via
//! `MatrixRequest::AiRoom`; results come back to the UI thread as
//! [`AiRoomAction`]s, applied in `a2app::runtime`.
//!
//! See [`crate::a2app::ai_room_events`] for the wire types shared with the
//! (cross-platform) rendering side.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use makepad_widgets::*;
use matrix_sdk::Room;
use matrix_sdk::deserialized_responses::RawAnySyncOrStrippedState;
use matrix_sdk::ruma::{
    OwnedEventId, OwnedRoomId,
    api::client::room::create_room,
    assign,
    events::{
        AnyInitialStateEvent, AnyRoomAccountDataEventContent, InitialStateEvent,
        RoomAccountDataEventType, StateEventType,
        room::encryption::RoomEncryptionEventContent,
    },
    serde::Raw,
};

use crate::a2app::ai_room_events::{
    AI_REPLY_EVENT_TYPE, AI_ROOM_EVENT_TYPE, AI_SESSION_DATA_EVENT_TYPE,
    AiReplyContent, AiRoomMarkerContent, AiSessionCursorContent,
};
use crate::utils::RoomNameId;

/// Matrix operations for AI rooms, dispatched via `MatrixRequest::AiRoom`
/// and run on the async worker's tokio runtime.
#[derive(Debug)]
pub enum AiRoomRequest {
    /// Creates a new private room and marks it as an AI room in the same
    /// request (the marker rides along in `initial_state`), so there's no
    /// window where the room exists but isn't yet an AI room.
    Create { name: String },
    /// Reads the marker (and forwarding cursor, if present) for a room that
    /// was just opened, so the runtime can attach its session.
    CheckMarker { room_id: OwnedRoomId },
    /// Persists the last-forwarded event id, best-effort: a failure here
    /// just means a restart re-forwards a few already-answered messages.
    SaveCursor { room_id: OwnedRoomId, cursor: OwnedEventId },
    /// Writes one agent turn (a completed reply, or a `send_message` tool
    /// call) as an `ai_reply` state event.
    PostReply { room_id: OwnedRoomId, content: AiReplyContent },
}

/// What [`handle_ai_room_request`] reports back to the UI thread.
#[derive(Clone, Debug)]
pub enum AiRoomAction {
    /// A new AI room was created; navigate to it.
    Created { room_name_id: RoomNameId },
    CreateFailed { error: String },
    /// The room has the marker: attach (or keep) its session.
    Attached { room_id: OwnedRoomId, name: Option<String>, cursor: Option<OwnedEventId> },
    /// The room has no marker: it's an ordinary room.
    NotAiRoom { room_id: OwnedRoomId },
    /// An `ai_reply` failed to post; the turn's text is otherwise lost, same
    /// as any other failed send.
    PostReplyFailed { error: String },
}

/// Runs one AI-room Matrix operation on the async worker.
pub async fn handle_ai_room_request(request: AiRoomRequest) {
    use crate::sliding_sync::get_client;
    match request {
        AiRoomRequest::Create { name } => {
            log!("AI Rooms worker: creating AI room \"{name}\"...");
            Cx::post_action(create_ai_room(&name).await);
        }
        AiRoomRequest::CheckMarker { room_id } => {
            log!("AI Rooms worker: checking ai_room marker for {room_id}...");
            if let Some(action) = check_marker(&room_id).await {
                Cx::post_action(action);
            }
        }
        AiRoomRequest::SaveCursor { room_id, cursor } => {
            let Some(room) = get_client().and_then(|c| c.get_room(&room_id)) else {
                log!("AI Rooms worker: can't save cursor for {room_id}: room not found in client.");
                return;
            };
            if let Err(e) = save_cursor(&room, &cursor).await {
                warning!("Failed to save AI session cursor for room {room_id}: {e}");
            } else {
                log!("AI Rooms worker: saved forwarding cursor {cursor} for {room_id}.");
            }
        }
        AiRoomRequest::PostReply { room_id, content } => {
            let Some(room) = get_client().and_then(|c| c.get_room(&room_id)) else {
                log!("AI Rooms worker: can't post ai_reply to {room_id}: room not found in client.");
                return;
            };
            if let Err(e) = post_reply(&room, &content).await {
                log!("AI Rooms worker: FAILED to post ai_reply to {room_id}: {e}");
                Cx::post_action(AiRoomAction::PostReplyFailed { error: e });
            }
        }
    }
}

async fn create_ai_room(name: &str) -> AiRoomAction {
    let Some(client) = crate::sliding_sync::get_client() else {
        return AiRoomAction::CreateFailed { error: "not logged in".into() };
    };
    let marker = serde_json::json!({
        "type": AI_ROOM_EVENT_TYPE,
        "state_key": "",
        "content": AiRoomMarkerContent { v: 1, name: Some(name.to_owned()) },
    });
    let raw_marker: Raw<AnyInitialStateEvent> = match Raw::new(&marker) {
        Ok(raw) => raw.cast_unchecked(),
        Err(e) => return AiRoomAction::CreateFailed { error: e.to_string() },
    };
    // Same defaults as `create_dm`: an AI room is a private, encrypted
    // conversation (robrix always builds matrix-sdk with e2e-encryption on).
    let initial_state = vec![
        InitialStateEvent::with_empty_state_key(
            RoomEncryptionEventContent::with_recommended_defaults(),
        ).to_raw_any(),
        raw_marker,
    ];

    let request = assign!(create_room::v3::Request::new(), {
        name: Some(name.to_owned()),
        preset: Some(create_room::v3::RoomPreset::PrivateChat),
        initial_state,
    });
    match client.create_room(request).await {
        Ok(room) => AiRoomAction::Created { room_name_id: RoomNameId::from_room(&room).await },
        Err(e) => {
            log!("AI Rooms worker: FAILED to create AI room \"{name}\": {e}");
            AiRoomAction::CreateFailed { error: e.to_string() }
        }
    }
}

/// Checks whether a room carries the AI-room marker. Returns `None` (and
/// posts nothing) when the room can't be positively classified, so the room
/// is never cached as non-AI on incomplete data — the next open re-checks.
///
/// Two separate waits are needed for a brand-new room: the room object
/// itself can lag sliding sync for a moment after `create_room`, and its
/// state events (including our `ai_room` marker, written via the create
/// request's `initial_state`) stream in *after* the room object appears.
/// Reading state before that would miss the marker and wrongly classify the
/// room as ordinary, so a marker miss only counts once the room's state has
/// demonstrably synced (its `m.room.create` event is cached locally).
async fn check_marker(room_id: &OwnedRoomId) -> Option<AiRoomAction> {
    // Wait for the room object itself, then for its synced state.
    let mut room = crate::sliding_sync::get_client().and_then(|c| c.get_room(room_id));
    for _ in 0..10 {
        if room.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        room = crate::sliding_sync::get_client().and_then(|c| c.get_room(room_id));
    }
    let Some(room) = room else {
        log!("AI Rooms worker: room {room_id} still not in the client after retries; deferring the marker check (re-checked when the room is next opened).");
        return None;
    };
    for _ in 0..20 {
        if let Some(marker) = read_marker(&room).await {
            let cursor = read_cursor(&room).await
                .and_then(|c| c.cursor)
                .and_then(|c| <&matrix_sdk::ruma::EventId>::try_from(c.as_str()).ok().map(ToOwned::to_owned));
            log!("AI Rooms worker: room {room_id} IS an AI room (marker name: {:?}, saved cursor: {:?}).", marker.name, cursor);
            return Some(AiRoomAction::Attached { room_id: room_id.clone(), name: marker.name, cursor });
        }
        let state_synced = room
            .get_state_event(StateEventType::RoomCreate, "")
            .await
            .ok()
            .flatten()
            .is_some();
        if state_synced {
            log!("AI Rooms worker: room {room_id} exists and its state has synced (m.room.create cached), but it carries no ai_room marker event.");
            return Some(AiRoomAction::NotAiRoom { room_id: room_id.clone() });
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    log!("AI Rooms worker: room {room_id}'s state never synced within the retry window; deferring the marker check (re-checked when the room is next opened).");
    None
}

/// Reads the raw JSON of a `Raw<AnySyncStateEvent>`/`Raw<AnyStrippedStateEvent>`,
/// regardless of which one a room's state actually holds.
fn deserialize_raw_state(raw: &RawAnySyncOrStrippedState) -> serde_json::Result<serde_json::Value> {
    match raw {
        RawAnySyncOrStrippedState::Sync(r) => r.deserialize_as_unchecked(),
        RawAnySyncOrStrippedState::Stripped(r) => r.deserialize_as_unchecked(),
    }
}

/// Reads the AI-room marker, if any. The marker's `v` field must be present
/// for the room to count; an emptied `{}` content — this module's convention
/// for "un-marking" a room — does not.
async fn read_marker(room: &Room) -> Option<AiRoomMarkerContent> {
    let raw = room.get_state_event(StateEventType::from(AI_ROOM_EVENT_TYPE), "").await.ok()??;
    let json = deserialize_raw_state(&raw).ok()?;
    serde_json::from_value(json.get("content")?.clone()).ok()
}

/// Reads this room's forwarding-cursor account data, if any.
async fn read_cursor(room: &Room) -> Option<AiSessionCursorContent> {
    let raw = room.account_data(RoomAccountDataEventType::from(AI_SESSION_DATA_EVENT_TYPE)).await.ok()??;
    let json: serde_json::Value = raw.deserialize_as_unchecked().ok()?;
    serde_json::from_value(json.get("content")?.clone()).ok()
}

/// Persists the last-forwarded event id as room account data.
async fn save_cursor(room: &Room, cursor: &OwnedEventId) -> Result<(), String> {
    let content = AiSessionCursorContent { cursor: Some(cursor.to_string()), last_turn: None };
    let raw_content: Raw<AnyRoomAccountDataEventContent> = Raw::new(&content)
        .map_err(|e| e.to_string())?
        .cast_unchecked();
    room.set_account_data_raw(RoomAccountDataEventType::from(AI_SESSION_DATA_EVENT_TYPE), raw_content)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The next unique `ai_reply` state key. Nanosecond timestamp plus a counter
/// keeps keys unique within (and, for all practical purposes, across) this
/// process's lifetime without needing an extra `uuid` dependency.
static NEXT_REPLY_KEY: AtomicU64 = AtomicU64::new(0);

fn next_reply_state_key() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
    let n = NEXT_REPLY_KEY.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{n:x}")
}

/// Writes one agent turn as an `ai_reply` state event with a fresh state key
/// (so it never overwrites a previous turn's reply).
async fn post_reply(room: &Room, content: &AiReplyContent) -> Result<(), String> {
    let json = serde_json::to_value(content).map_err(|e| e.to_string())?;
    let key = next_reply_state_key();
    match room.send_state_event_raw(AI_REPLY_EVENT_TYPE, &key, json).await {
        Ok(response) => {
            log!("AI Rooms worker: wrote ai_reply state event key {key} -> {} in room {}.", response.event_id, room.room_id());
            Ok(())
        }
        Err(e) => {
            log!("AI Rooms worker: ai_reply write to room {} failed: {e}", room.room_id());
            Err(e.to_string())
        }
    }
}
