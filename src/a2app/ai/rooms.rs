//! AI Rooms: the Matrix operations that back the marker/forwarding/reply
//! machinery (create a room, read/write its marker, read/write its
//! forwarding cursor, write an `ai_reply`, post live `ai_activity` /
//! `ai_tool_call` rows). Run on the async worker via `MatrixRequest::AiRoom`;
//! results come back to the UI thread as [`AiRoomAction`]s, applied in
//! `a2app::runtime`.
//!
//! See [`crate::a2app::ai_room_events`] for the wire types shared with the
//! (cross-platform) rendering side.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use makepad_widgets::*;
use matrix_sdk::Room;
use matrix_sdk::deserialized_responses::RawAnySyncOrStrippedState;
use matrix_sdk::ruma::{
    OwnedEventId, OwnedRoomId, UserId,
    api::client::{room::create_room, state::{get_state_event_for_key, get_state_events}},
    assign,
    events::{
        AnyInitialStateEvent, AnyRoomAccountDataEventContent, InitialStateEvent,
        RoomAccountDataEventType, StateEventType,
        room::encryption::RoomEncryptionEventContent,
        room::message::RoomMessageEventContent,
    },
    serde::Raw,
};

use crate::a2app::ai::tools::{ReadToolKind, read_tool_name};
use crate::a2app::ai_room_events::{
    AI_REPLY_EVENT_TYPE, AI_ROOM_EVENT_TYPE, AI_SESSION_DATA_EVENT_TYPE,
    AiReplyContent, AiRoomMarkerContent, AiSessionCursorContent,
};
use crate::utils::RoomNameId;
use crate::a2app::matrix::{MatrixAuthorization, policy};
use a2app_core::permissions::RoomAccess;
use a2app_core::information_flow::{self as flow, ContextId, Recipient};

/// Counter backing [`next_ai_state_key`], so two rows minted in the same
/// nanosecond never collide.
static NEXT_AI_EVENT_KEY: AtomicU64 = AtomicU64::new(1);

/// Mints a fresh, unique state key for one `ai_activity` / `ai_tool_call`
/// row. The prefix names the kind of row the key belongs to; the key is
/// unique per row, and reusing one (a tool call's `Started` → `Done` update)
/// rewrites that row in the room's current state instead of appending.
///
/// UI-thread safe: only the atomic counter is shared.
pub fn next_ai_state_key(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let n = NEXT_AI_EVENT_KEY.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{nanos:x}-{n:x}")
}

/// Matrix operations for AI rooms, dispatched via `MatrixRequest::AiRoom`
/// and run on the async worker's tokio runtime.
#[derive(Debug)]
pub enum AiRoomRequest {
    /// Creates a new private room and marks it as an AI room in the same
    /// request (the marker rides along in `initial_state`), so there's no
    /// window where the room exists but isn't yet an AI room.
    Create { name: String },
    /// Writes the marker to an existing room (`/ai enable`), so a room that
    /// was created before markers existed — or by another client — becomes
    /// an AI room. The room's agent then attaches like any marked room.
    Mark { room_id: OwnedRoomId },
    /// Reads the marker (and forwarding cursor, if present) for a room that
    /// was just opened, so the runtime can attach its session.
    CheckMarker { room_id: OwnedRoomId },
    /// Persists the last-forwarded event id, best-effort: a failure here
    /// just means a restart re-forwards a few already-answered messages.
    SaveCursor { room_id: OwnedRoomId, cursor: OwnedEventId, flow_epoch: u64, flow_context: ContextId },
    /// Writes one agent turn (a completed reply, or a `send_message` tool
    /// call) as an `ai_reply` state event. `answer_id` is set when a tool
    /// call is parked on the write: the room may refuse it (too little state
    /// power), and the model must be told that rather than "posted".
    PostReply {
        room_id: OwnedRoomId,
        content: AiReplyContent,
        answer_id: Option<u64>,
        flow_epoch: u64, flow_context: ContextId,
    },
    /// Posts one agent turn into ANOTHER joined room as an `m.notice`
    /// message (the `post_room_message` tool). Unlike an `ai_reply` state
    /// event, this needs no state-power privilege — it's an ordinary message
    /// the account may send. Answers the parked tool call through
    /// [`AiRoomAction::PostToRoomResult`] once the write lands.
    PostToRoom {
        id: u64,
        target: OwnedRoomId,
        content: AiReplyContent,
        flow_epoch: u64, flow_context: ContextId,
    },
    /// Writes one raw AI-session activity state event (an `ai_activity`
    /// marker or an `ai_tool_call` row). The caller supplies the state key:
    /// a fresh key (see [`next_ai_state_key`]) appends a new row to the
    /// room's live log; reusing a key — a tool call's `Started` → `Done`
    /// update — rewrites that one row. Best-effort: a failure is logged and
    /// the turn continues (the final `ai_reply` still carries the receipts).
    PostAiStateEvent {
        room_id: OwnedRoomId,
        event_type: String,
        state_key: String,
        content: serde_json::Value,
        flow_epoch: u64, flow_context: ContextId,
    },
    /// A capability-gated attached-room read the session's agent asked for
    /// (already decided Granted by the runtime against the room's permission
    /// subject). Fetches the data and posts the result back as
    /// [`AiRoomAction::ToolReadResult`] under `id`, which the runtime uses to
    /// answer the parked tool call.
    ToolRead { id: u64, room_id: OwnedRoomId, tool: ReadToolKind, authorization: Option<MatrixAuthorization>, flow_epoch: u64, flow_context: ContextId },
}

/// What [`handle_ai_room_request`] reports back to the UI thread.
#[derive(Clone, Debug)]
pub enum AiRoomAction {
    /// A new AI room was created; navigate to it.
    Created { room_name_id: RoomNameId },
    CreateFailed { error: String },
    /// A `/ai enable` marker write failed; surface why.
    MarkFailed { error: String },
    /// The room has the marker: attach (or keep) its session.
    Attached { room_id: OwnedRoomId, name: Option<String>, cursor: Option<OwnedEventId> },
    /// The room has no marker: it's an ordinary room.
    NotAiRoom { room_id: OwnedRoomId },
    /// An `ai_reply` write finished. `answer_id` carries the parked
    /// `send_message` tool call, if one is waiting on it.
    PostReplyResult {
        room_id: OwnedRoomId,
        answer_id: Option<u64>,
        result: Result<(), String>,
    },
    /// A granted [`AiRoomRequest::PostToRoom`] finished; `result` is the text
    /// (or the error) the waiting tool call must be answered with.
    PostToRoomResult { id: u64, result: Result<String, String> },
    /// A granted [`AiRoomRequest::ToolRead`] finished; `result` is the JSON
    /// text (or the error) the waiting tool call must be answered with.
    ToolReadResult { id: u64, result: Result<String, String>, authorization: Option<MatrixAuthorization> },
    /// A state-event write finished (succeeded or failed). The runtime uses
    /// this to release the room's `ai_turn` in-flight flag — and to widen or
    /// narrow its write spacing — so the next coalesced turn snapshot can go
    /// out without piling onto the server's state-event rate limit.
    StateEventPosted { room_id: OwnedRoomId, event_type: String, success: bool },
}

/// Raw Matrix state remains plaintext even in encrypted rooms.
///
/// Room-output consent alone does not release private content to the
/// homeserver. Check its exact origin separately, using captured provenance.
fn ensure_ai_state_output(room: &Room, flow_context: &ContextId, action: &str) -> Result<(), String> {
    policy::ensure_room_flow_output(flow_context, room.room_id().as_str())?;
    policy::ensure_flow_action(flow_context, &flow::SensitiveAction {
        kind: action.into(), target: room.room_id().to_string(),
    })?;
    let homeserver = room.client().homeserver();
    let recipient = Recipient::network_origin(homeserver.as_str())?;
    let origin = homeserver.origin().ascii_serialization();
    policy::ensure_flow_output(flow_context, &recipient).map_err(|error| format!(
        "{error} AI reply and activity cards are unencrypted Matrix state; their source sharing rules must also allow the homeserver origin {origin}."
    ))
}

/// Sends one best-effort AI state row without HTTP retry/backoff.
///
/// The coalescing runtime submits later snapshots through a fresh policy
/// check instead of retaining an unbounded rate-limit retry.
async fn send_ai_state_event(
    room: &Room,
    event_type: &str,
    state_key: &str,
    content: serde_json::Value,
    flow_context: &ContextId,
) -> Result<(), String> {
    policy::ensure_room_access(room.room_id().as_str(), RoomAccess::Write)?;
    use matrix_sdk::ruma::api::client::state::send_state_event;
    use matrix_sdk::utils::IntoRawStateEventContent;
    let request = send_state_event::v3::Request::new_raw(
        room.room_id().to_owned(),
        event_type.into(),
        state_key.to_owned(),
        content.into_raw_state_event_content(),
    );
    let config = room.client().request_config().disable_retry();
    ensure_ai_state_output(room, flow_context, "ai.activity.write")?;
    room.client()
        .send(request)
        .with_request_config(config)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Runs one AI-room Matrix operation on the async worker.
pub async fn handle_ai_room_request(request: AiRoomRequest) {
    let activation = match &request {
        AiRoomRequest::SaveCursor { flow_context, flow_epoch, .. }
        | AiRoomRequest::PostReply { flow_context, flow_epoch, .. }
        | AiRoomRequest::PostToRoom { flow_context, flow_epoch, .. }
        | AiRoomRequest::PostAiStateEvent { flow_context, flow_epoch, .. }
        | AiRoomRequest::ToolRead { flow_context, flow_epoch, .. } => Some((flow_context.clone(), *flow_epoch)),
        AiRoomRequest::Create { .. } | AiRoomRequest::Mark { .. } | AiRoomRequest::CheckMarker { .. } => None,
    };
    if let Some((context, epoch)) = activation {
        policy::with_flow_activation(context, epoch, handle_ai_room_request_inner(request)).await;
    } else {
        handle_ai_room_request_inner(request).await;
    }
}

async fn handle_ai_room_request_inner(request: AiRoomRequest) {
    use crate::sliding_sync::get_client;
    match request {
        AiRoomRequest::Create { name } => {
            log!("AI Rooms worker: creating AI room \"{name}\"...");
            Cx::post_action(create_ai_room(&name).await);
        }
        AiRoomRequest::Mark { room_id } => {
            log!("AI Rooms worker: marking {room_id} as an AI room...");
            let Some(room) = get_client().and_then(|c| c.get_room(&room_id)) else {
                log!("AI Rooms worker: can't mark {room_id}: room not found in client.");
                Cx::post_action(AiRoomAction::MarkFailed { error: "room not found in client".into() });
                return;
            };
            let content = AiRoomMarkerContent { v: 1, name: None };
            if let Err(e) = write_marker(&room, &content).await {
                log!("AI Rooms worker: FAILED to mark {room_id} as an AI room: {e}");
                Cx::post_action(AiRoomAction::MarkFailed { error: e });
                return;
            }
            log!("AI Rooms worker: wrote the ai_room marker for {room_id}.");
            // Attach exactly like a room whose marker was found on open (the
            // room was just marked, so there is no saved forwarding cursor yet).
            Cx::post_action(AiRoomAction::Attached { room_id, name: None, cursor: None });
        }
        AiRoomRequest::CheckMarker { room_id } => {
            log!("AI Rooms worker: checking ai_room marker for {room_id}...");
            if let Some(action) = check_marker(&room_id).await {
                Cx::post_action(action);
            }
        }
        AiRoomRequest::SaveCursor { room_id, cursor, flow_context, .. } => {
            let Some(room) = get_client().and_then(|c| c.get_room(&room_id)) else {
                log!("AI Rooms worker: can't save cursor for {room_id}: room not found in client.");
                return;
            };
            if let Err(e) = save_cursor(&room, &cursor, &flow_context).await {
                warning!("Failed to save AI session cursor for room {room_id}: {e}");
            } else {
                log!("AI Rooms worker: saved forwarding cursor {cursor} for {room_id}.");
            }
        }
        AiRoomRequest::PostReply { room_id, content, answer_id, flow_context, .. } => {
            let Some(room) = get_client().and_then(|c| c.get_room(&room_id)) else {
                let msg = format!("room {room_id} not found in client");
                log!("AI Rooms worker: can't post ai_reply to {room_id}: {msg}");
                Cx::post_action(AiRoomAction::PostReplyResult { room_id, answer_id, result: Err(msg) });
                return;
            };
            let result = post_reply(&room, &content, &flow_context).await;
            if let Err(e) = &result {
                log!("AI Rooms worker: FAILED to post ai_reply to {room_id}: {e}");
            }
            Cx::post_action(AiRoomAction::PostReplyResult { room_id, answer_id, result });
        }
        AiRoomRequest::PostToRoom { id, target, content, flow_context, .. } => {
            let Some(room) = get_client().and_then(|c| c.get_room(&target)) else {
                let msg = format!("room {target} not found in client (are you still joined?)");
                log!("AI Rooms worker: can't post a notice to {target}: {msg}");
                Cx::post_action(AiRoomAction::PostToRoomResult { id, result: Err(msg) });
                return;
            };
            match post_notice(&room, &content, &flow_context).await {
                Ok(_) => {
                    Cx::post_action(AiRoomAction::PostToRoomResult {
                        id,
                        result: Ok(String::from("Posted to the room as a notice.")),
                    });
                }
                Err(e) => {
                    log!("AI Rooms worker: FAILED to post a notice to {target}: {e}");
                    Cx::post_action(AiRoomAction::PostToRoomResult { id, result: Err(e) });
                }
            }
        }
        AiRoomRequest::PostAiStateEvent { room_id, event_type, state_key, content, flow_context, .. } => {
            let Some(room) = get_client().and_then(|c| c.get_room(&room_id)) else {
                log!("AI Rooms worker: can't post {event_type} to {room_id}: room not found in client.");
                // Still release the runtime's in-flight flag; the row is lost
                // but the turn must not wedge behind it.
                Cx::post_action(AiRoomAction::StateEventPosted { room_id, event_type, success: false });
                return;
            };
            let success = match send_ai_state_event(&room, &event_type, &state_key, content, &flow_context).await {
                Ok(()) => true,
                Err(e) => {
                    // Best-effort rows: a failure must not derail the turn (the
                    // final `ai_reply` still carries the receipts), so log only.
                    log!("AI Rooms worker: FAILED to post {event_type} (key {state_key}) to {room_id}: {e}");
                    false
                }
            };
            Cx::post_action(AiRoomAction::StateEventPosted { room_id, event_type, success });
        }
        AiRoomRequest::ToolRead { id, room_id, tool, authorization, flow_context, .. } => {
            let read = async {
                policy::ensure_live_activation()?;
                crate::a2app::information_flow::current_context(&flow_context)?;
                let target = match &tool {
                    ReadToolKind::OtherRoom { room, .. } => Some(room.as_str()),
                    ReadToolKind::SpaceInfo { space } | ReadToolKind::SpaceRooms { space } => Some(space.as_str()),
                    ReadToolKind::ListRooms | ReadToolKind::ListSpaces => None,
                    _ => Some(room_id.as_str()),
                };
                if let Some(target) = target {
                    policy::ensure_room_flow_output(&flow_context, target)?;
                    policy::ensure_room_access(target, RoomAccess::Read)?;
                }
                let result = read_tool(&room_id, tool.clone(), &flow_context).await?;
                policy::ensure_live_activation()?;
                crate::a2app::information_flow::current_context(&flow_context)?;
                if let Some(target) = target {
                    policy::ensure_room_flow_output(&flow_context, target)?;
                    policy::ensure_room_access(target, RoomAccess::Read)?;
                }
                policy::filter_current_read_result(&result)
            };
            let result = match &authorization {
                Some(authorization) => policy::with_authorization(authorization.clone(), read).await,
                None => read.await,
            };
            match &result {
                Ok(text) => log!(
                    "AI Rooms worker: {} for {} returned OK ({} chars).",
                    read_tool_name(&tool),
                    room_id,
                    text.chars().count()
                ),
                Err(e) => log!(
                    "AI Rooms worker: {} for {} returned Err: {}",
                    read_tool_name(&tool),
                    room_id,
                    e
                ),
            }
            Cx::post_action(AiRoomAction::ToolReadResult { id, result, authorization });
        }
    }
}

/// Runs one granted attached-room read against the same matrix machinery the
/// mini-app services use (`matrix::room::*`), returning the same JSON text
/// shapes a mini-app's `host.request` would get.
async fn read_tool(room_id: &OwnedRoomId, tool: ReadToolKind, flow_context: &ContextId) -> Result<String, String> {
    use crate::a2app::matrix::room as matrix_room;
    use crate::a2app::matrix::rooms as matrix_rooms;
    use crate::a2app::matrix::spaces as matrix_spaces;
    match tool {
        // The AI's read tools return full message bodies (the model quotes
        // and reasons about what a human wrote); only the mini-app services
        // clip.
        ReadToolKind::Messages { limit } => matrix_room::read_messages(room_id.clone(), limit, true).await,
        ReadToolKind::Older { before, limit } => {
            let before = before
                .as_deref()
                .map(OwnedEventId::try_from)
                .transpose()
                .map_err(|_| "not a valid event id")?;
            matrix_room::older_messages(room_id.clone(), before, limit, true).await
        }
        ReadToolKind::Info => matrix_room::info(room_id.clone()).await,
        // The target room was granted on the UI thread; fetch from it
        // directly (it must be a joined room or the read answers "not
        // found"/"room not joined").
        ReadToolKind::OtherRoom { room, limit } => {
            let target = OwnedRoomId::try_from(room.as_str()).map_err(|_| "not a valid room id")?;
            matrix_room::read_messages(target, limit, true).await
        }
        ReadToolKind::ListRooms => matrix_rooms::joined_rooms_list().await,
        ReadToolKind::ListSpaces => matrix_spaces::list().await,
        ReadToolKind::SpaceInfo { space } => {
            let space = OwnedRoomId::try_from(space.as_str()).map_err(|_| "not a valid space id")?;
            matrix_spaces::info(space).await
        }
        ReadToolKind::SpaceRooms { space } => {
            let space = OwnedRoomId::try_from(space.as_str()).map_err(|_| "not a valid space id")?;
            matrix_spaces::rooms(space).await
        }
        // Ungated plumbing (no capability): the agent recalling its own turns.
        ReadToolKind::Memory { limit } => room_memory(room_id, limit, flow_context).await,
    }
}

/// Fetches the room's recent `ai_reply` state events — the agent's own past
/// turns — oldest first, one JSON row per turn: the reply text, its
/// timestamp, the model that wrote it, the tool calls its turn made, and
/// which user message it answered. Same event-cache-then-`/messages` walk
/// the message reads use; the replies are ordinary state events in the
/// timeline, so both sources carry them. The text is deliberately NOT
/// clipped: this is the agent's memory of its own work, and continuing that
/// work needs it whole.
async fn room_memory(room_id: &OwnedRoomId, limit: u32, flow_context: &ContextId) -> Result<String, String> {
    use matrix_sdk::room::MessagesOptions;
    use matrix_sdk::deserialized_responses::TimelineEvent;

    /// One past turn as a JSON row; `true` once `limit` rows are collected.
    fn push_turn(out: &mut Vec<serde_json::Value>, event: &TimelineEvent, limit: usize, me: &str) -> bool {
        // Custom state types (the ai_reply event) aren't in the typed event
        // enum, so read the raw JSON and filter on the type string.
        let Ok(raw) = event.raw().deserialize_as::<serde_json::Value>() else {
            return out.len() >= limit;
        };
        if raw.get("type").and_then(serde_json::Value::as_str) != Some(AI_REPLY_EVENT_TYPE) {
            return out.len() >= limit;
        }
        // Only this account's own turns are the agent's memory: another
        // member with state power could write the type.
        if raw.get("sender").and_then(serde_json::Value::as_str) != Some(me) {
            return out.len() >= limit;
        }
        let Some(content) = raw.get("content").cloned() else {
            return out.len() >= limit;
        };
        let Ok(content) = serde_json::from_value::<AiReplyContent>(content) else {
            return out.len() >= limit;
        };
        out.push(serde_json::json!({
            "event_id": raw.get("event_id").and_then(serde_json::Value::as_str),
            "ts": content.created_at,
            "text": content.text,
            "model": content.model,
            "tool_calls": content.tool_calls,
            "in_reply_to": content.in_reply_to,
        }));
        out.len() >= limit
    }

    let client = crate::sliding_sync::get_client().ok_or("not logged in")?;
    let me = crate::sliding_sync::current_user_id().ok_or("not logged in")?;
    let room = client.get_room(room_id).ok_or("room not found")?;
    let limit = (limit as usize).clamp(1, 50);
    let mut out: Vec<serde_json::Value> = Vec::new();
    // The event cache already holds the recent timeline; only hit the
    // network when it can't fill the request.
    if let Ok((cache, _guard)) = client.event_cache().room(room_id).await
        && let Ok(events) = cache.events().await
    {
        for event in events.iter().rev() {
            if push_turn(&mut out, event, limit, me.as_str()) {
                break;
            }
        }
    }
    if out.len() < limit {
        // A room's recent tail can be all regular messages, so keep
        // paginating until we fill `limit`. Start over rather than mixing
        // the two sources (their newest page may overlap the cache).
        out.clear();
        let mut from: Option<String> = None;
        for _ in 0..4 {
            policy::ensure_room_access(room_id.as_str(), RoomAccess::Read)?;
            policy::ensure_room_flow_output(flow_context, room_id.as_str())?;
            let mut options = MessagesOptions::backward();
            options.limit = 50u32.into();
            options.from = from;
            let messages = room.messages(options).await
                .map_err(|e| format!("couldn't load the room's past turns: {e}"))?;
            for event in messages.chunk {
                if push_turn(&mut out, &event, limit, me.as_str()) {
                    break;
                }
            }
            from = messages.end;
            if out.len() >= limit || from.is_none() {
                break;
            }
        }
    }
    // Pagination answers newest-first; the agent reads oldest-first.
    out.reverse();
    Ok(serde_json::json!({ "turns": out }).to_string())
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
        Ok(room) => {
            // Homeservers (Synapse included) only honor a small allowlist of
            // *known* event types in create_room's initial_state and silently
            // drop a custom marker event, so also write the marker through the
            // ordinary state-event endpoint — the same one ai_reply events
            // use, which demonstrably round-trips through sync. The room is
            // recorded as an AI room in-memory at `Created` either way; this
            // write is what makes it survive an app restart.
            let content = AiRoomMarkerContent { v: 1, name: Some(name.to_owned()) };
            if let Err(e) = write_marker(&room, &content).await {
                log!("AI Rooms worker: WARNING: couldn't persist the ai_room marker on the new room; it will only be an AI room until the app restarts: {e}");
            }
            AiRoomAction::Created { room_name_id: RoomNameId::from_room(&room).await }
        }
        Err(e) => {
            log!("AI Rooms worker: FAILED to create AI room \"{name}\": {e}");
            AiRoomAction::CreateFailed { error: e.to_string() }
        }
    }
}

/// Writes the ai_room marker state event once the room exists. Retries while
/// the brand-new room settles in sliding sync: right after `create_room` the
/// local client can lag a moment (the same lag `check_marker` waits out for
/// reads), and sending state requires the room to be joined locally.
async fn write_marker(room: &Room, content: &AiRoomMarkerContent) -> Result<(), String> {
    let json = serde_json::to_value(content).map_err(|e| e.to_string())?;
    let mut last_err = String::from("state send never attempted");
    for _ in 0..10 {
        match room.send_state_event_raw(AI_ROOM_EVENT_TYPE, "", json.clone()).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                last_err = e.to_string();
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
        }
    }
    Err(last_err)
}

/// Checks whether a room carries the AI-room marker. Returns `None` (and
/// posts nothing) when the room can't be positively classified, so the room
/// is never cached as non-AI on incomplete data — the next open re-checks.
///
/// Two separate waits are needed for a brand-new room: the room object
/// itself can lag sliding sync for a moment after `create_room`, and its
/// state events stream in *after* the room object appears. Reading state
/// before that would miss the marker and wrongly classify the room as
/// ordinary, so a marker miss only counts once the room's state has
/// demonstrably synced (its `m.room.create` event is cached locally).
///
/// The marker never reaches the local state store through sliding sync at
/// all: the SDK only requests a fixed set of *known* state types
/// (`required_state`), and custom types like the ai_room marker are not in
/// it. A local miss is therefore confirmed against the homeserver (which is
/// authoritative) before the room is classified as ordinary.
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
            return Some(attached_action(&room, room_id, marker).await);
        }
        let state_synced = room
            .get_state_event(StateEventType::RoomCreate, "")
            .await
            .ok()
            .flatten()
            .is_some();
        if state_synced {
            // The local store has synced and has no marker, but that only
            // proves sliding sync didn't deliver it (custom types never are).
            // Ask the homeserver directly; a `NotFound` there is the one
            // authoritative "not an AI room", and a transient fetch failure
            // just retries below without classifying the room.
            match read_marker_from_server(&room).await {
                Ok(Some(marker)) => return Some(attached_action(&room, room_id, marker).await),
                Ok(None) => {
                    log!("AI Rooms worker: room {room_id} exists and its state has synced (m.room.create cached), but the homeserver reports no ai_room marker event.");
                    return Some(AiRoomAction::NotAiRoom { room_id: room_id.clone() });
                }
                Err(e) => {
                    log!("AI Rooms worker: couldn't confirm the ai_room marker for {room_id} from the homeserver ({e}); retrying.");
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    log!("AI Rooms worker: room {room_id}'s marker could not be confirmed within the retry window; deferring the marker check (re-checked when the room is next opened).");
    None
}

/// Builds the `Attached` action for a room whose marker was found, reading
/// the saved forwarding cursor as it goes.
async fn attached_action(
    room: &Room,
    room_id: &OwnedRoomId,
    marker: AiRoomMarkerContent,
) -> AiRoomAction {
    let cursor = read_cursor(room).await
        .and_then(|c| c.cursor)
        .and_then(|c| <&matrix_sdk::ruma::EventId>::try_from(c.as_str()).ok().map(ToOwned::to_owned));
    log!("AI Rooms worker: room {room_id} IS an AI room (marker name: {:?}, saved cursor: {:?}).", marker.name, cursor);
    AiRoomAction::Attached { room_id: room_id.clone(), name: marker.name, cursor }
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
    let me = crate::sliding_sync::current_user_id()?;
    let raw = room.get_state_event(StateEventType::from(AI_ROOM_EVENT_TYPE), "").await.ok()??;
    let json = deserialize_raw_state(&raw).ok()?;
    marker_from_event(&json, &me)
}

/// The marker inside a full state event, if it is one this client should act
/// on: only the local user's own marker attaches an agent here. Any member
/// with state power can write the type, and a marker from someone else must
/// not start a session on this account (nor a second agent in a shared room).
fn marker_from_event(event: &serde_json::Value, me: &UserId) -> Option<AiRoomMarkerContent> {
    if event.get("sender").and_then(serde_json::Value::as_str) != Some(me.as_str()) {
        log!("AI Rooms worker: ignoring an ai_room marker sent by {:?} (not this account).", event.get("sender"));
        return None;
    }
    serde_json::from_value(event.get("content")?.clone()).ok()
}

/// Reads the marker straight from the homeserver, bypassing the local state
/// store. Sliding sync never delivers custom state types (they aren't in the
/// SDK's `required_state`), so the local store can't answer this reliably;
/// the homeserver is authoritative. `Ok(None)` means the marker is genuinely
/// absent (a 404), `Err` is a transient fetch failure.
async fn read_marker_from_server(room: &Room) -> Result<Option<AiRoomMarkerContent>, String> {
    use matrix_sdk::ruma::api::error::ErrorKind;
    let me = crate::sliding_sync::current_user_id().ok_or("not logged in")?;
    // The full event, not just its content: the sender decides whether the
    // marker is ours to act on.
    let request = assign!(
        get_state_event_for_key::v3::Request::new(
            room.room_id().to_owned(),
            StateEventType::from(AI_ROOM_EVENT_TYPE),
            String::new(),
        ),
        { format: get_state_event_for_key::v3::StateEventFormat::Event }
    );
    match room.client().send(request).await {
        Ok(response) => {
            // A present but unparseable (or foreign) marker is an authoritative
            // "not an AI room for this client", not a transient failure.
            let Ok(event) = serde_json::from_str::<serde_json::Value>(response.event_or_content.get())
            else {
                return Ok(None);
            };
            if event.get("sender").is_some() {
                return Ok(marker_from_event(&event, &me));
            }
            // A server that predates `format=event` (Matrix v1.16) answers
            // with the content alone; the room's full state carries senders.
            read_marker_from_full_state(room, &me).await
        }
        Err(e) if e.client_api_error_kind() == Some(&ErrorKind::NotFound) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// The marker as found in the room's full state (`GET /rooms/{id}/state`),
/// for servers whose per-key endpoint can't return the event's sender.
async fn read_marker_from_full_state(
    room: &Room,
    me: &UserId,
) -> Result<Option<AiRoomMarkerContent>, String> {
    let request = get_state_events::v3::Request::new(room.room_id().to_owned());
    let response = room.client().send(request).await.map_err(|e| e.to_string())?;
    for raw in &response.room_state {
        let Ok(event) = raw.deserialize_as_unchecked::<serde_json::Value>() else { continue };
        if event.get("type").and_then(serde_json::Value::as_str) == Some(AI_ROOM_EVENT_TYPE)
            && event.get("state_key").and_then(serde_json::Value::as_str) == Some("")
        {
            return Ok(marker_from_event(&event, me));
        }
    }
    Ok(None)
}

/// Reads this room's forwarding-cursor account data, if any.
async fn read_cursor(room: &Room) -> Option<AiSessionCursorContent> {
    let raw = room.account_data(RoomAccountDataEventType::from(AI_SESSION_DATA_EVENT_TYPE)).await.ok()??;
    let json: serde_json::Value = raw.deserialize_as_unchecked().ok()?;
    serde_json::from_value(json.get("content")?.clone()).ok()
}

/// Persists the last-forwarded event id as room account data.
async fn save_cursor(room: &Room, cursor: &OwnedEventId, flow_context: &ContextId) -> Result<(), String> {
    policy::ensure_room_access(room.room_id().as_str(), RoomAccess::Write)?;
    let content = AiSessionCursorContent { cursor: Some(cursor.to_string()), last_turn: None };
    let raw_content: Raw<AnyRoomAccountDataEventContent> = Raw::new(&content)
        .map_err(|e| e.to_string())?
        .cast_unchecked();
    policy::ensure_room_flow_output(flow_context, room.room_id().as_str())?;
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
/// Turns a failed `ai_reply` state write into a message the caller (and the
/// room's owner, via the UI popup) can act on. A 403/M_FORBIDDEN here is
/// almost always a power-level shortfall: posting a custom state event
/// requires the account's power in that room to be at least the room's
/// `state_default` (50 = Moderator by default). Name that instead of echoing
/// the raw server error, which reads as a mystery to the agent (and to the
/// room's owner).
fn friendly_state_post_error(e: &matrix_sdk::Error) -> String {
    use matrix_sdk::ruma::api::error::ErrorKind;
    if matches!(e.client_api_error_kind(), Some(ErrorKind::Forbidden)) {
        return String::from(
            "the homeserver refused the state event: this account is not powerful enough in the \
             target room. Posting as an AI card is a custom state event, which needs at least the \
             room's state_default power level (normally 50 = Moderator). Promote this account to \
             Moderator in that room, or lower the room's state default, then ask me to post again.",
        );
    }
    e.to_string()
}

/// Writes one agent turn as an `ai_reply` state event with a fresh state key
/// (so it never overwrites a previous turn's reply).
async fn post_reply(room: &Room, content: &AiReplyContent, flow_context: &ContextId) -> Result<(), String> {
    policy::ensure_room_access(room.room_id().as_str(), RoomAccess::Write)?;
    let json = serde_json::to_value(content).map_err(|e| e.to_string())?;
    let key = next_reply_state_key();
    use matrix_sdk::ruma::api::client::state::send_state_event;
    use matrix_sdk::utils::IntoRawStateEventContent;
    if room.state() != matrix_sdk::RoomState::Joined {
        return Err("The target room is no longer joined.".into());
    }
    let request = send_state_event::v3::Request::new_raw(
        room.room_id().to_owned(), AI_REPLY_EVENT_TYPE.into(), key.clone(), json.into_raw_state_event_content(),
    );
    let config = room.client().request_config().disable_retry();
    ensure_ai_state_output(room, flow_context, "ai.reply.write")?;
    match room.client().send(request).with_request_config(config).await {
        Ok(response) => {
            log!("AI Rooms worker: wrote ai_reply state event key {key} -> {} in room {}.", response.event_id, room.room_id());
            Ok(())
        }
        Err(e) => {
            let friendly = friendly_state_post_error(&e.into());
            log!("AI Rooms worker: ai_reply write to room {} failed: {friendly}", room.room_id());
            Err(friendly)
        }
    }
}

/// Turns a failed `m.notice` write into a message the caller — and the
/// model, for cross-room posts — can act on. A 403/M_FORBIDDEN here means the
/// account simply isn't allowed to send messages in that room (its power is
/// below `events_default`, normally 0), so name that instead of echoing the
/// raw server error.
fn friendly_notice_post_error(e: &matrix_sdk::Error) -> String {
    use matrix_sdk::ruma::api::error::ErrorKind;
    if matches!(e.client_api_error_kind(), Some(ErrorKind::Forbidden)) {
        return String::from(
            "the homeserver refused the message: this account is not allowed to send messages \
             in that room. Check the room's permissions, then ask me to post again.",
        );
    }
    e.to_string()
}

/// Prefix prepended to a cross-room notice so its recipients can tell it was
/// posted by the room's AI agent (through the user's own account) rather than
/// typed by the user directly.
const NOTICE_PROVENANCE_PREFIX: &str = "🤖 Robrix AI: ";

/// Writes one agent turn as an `m.notice` message (a normal `m.room.message`
/// with `msgtype` `m.notice`) into ANOTHER joined room. Notices need no
/// state-power privilege, so a cross-room post works with an ordinary
/// `events_default` power level instead of `state_default` (50 = Moderator).
async fn post_notice(room: &Room, content: &AiReplyContent, flow_context: &ContextId) -> Result<(), String> {
    policy::ensure_room_access(room.room_id().as_str(), RoomAccess::Write)?;
    let message = match content.formatted.as_deref().filter(|html| !html.is_empty()) {
        Some(html) => {
            let body = format!("{NOTICE_PROVENANCE_PREFIX}{}", content.text);
            let html_body = format!("<strong>{NOTICE_PROVENANCE_PREFIX}</strong>{html}");
            RoomMessageEventContent::notice_html(body, html_body)
        }
        None => RoomMessageEventContent::notice_plain(format!("{NOTICE_PROVENANCE_PREFIX}{}", content.text)),
    };
    policy::ensure_room_flow_output(flow_context, room.room_id().as_str())?;
    policy::ensure_flow_action(flow_context, &flow::SensitiveAction {
        kind: "matrix.rooms.message.send".into(), target: room.room_id().to_string(),
    })?;
    match room.send(message).await {
        Ok(response) => {
            log!("AI Rooms worker: posted a notice -> {} in room {}.", response.response.event_id, room.room_id());
            Ok(())
        }
        Err(e) => {
            let friendly = friendly_notice_post_error(&e);
            log!("AI Rooms worker: notice write to room {} failed: {friendly}", room.room_id());
            Err(friendly)
        }
    }
}
