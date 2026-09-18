//! The mini-app matrix services: what `host.request("matrix.*", ...)` runs
//! against the SDK on the worker, and the request/result types that carry
//! it there and back.
//!
//! One file per domain (`room`, `rooms`, `account`, `send`, `membership`) holds the worker
//! bodies; this file owns the request enum, the broker-call mapping and the
//! dispatch. Add a service by adding its variant, its `request_for` arm and
//! its dispatch arm in the domain's section, and its body in the domain file.

use std::collections::HashSet;

use makepad_widgets::*;
use matrix_sdk::RoomState;
use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId, OwnedRoomOrAliasId, OwnedServerName, OwnedUserId};

use a2app_core::services::{MatrixServiceCall, Reply, SearchScope};
use a2app_core::services::matrix::RoomFlag;

use crate::a2app::{account_watch, room_watch};
use crate::shared::popup_list::{enqueue_popup_notification, PopupKind};

pub mod account;
pub mod membership;
pub mod room;
pub mod rooms;
pub mod send;
pub mod spaces;

/// Matrix work requested by a mini-app (or a share), run on the worker.
#[derive(Debug)]
pub enum A2AppMatrixRequest {
    RoomInfo { room_id: OwnedRoomId, reply: Reply },
    ReadMessages { room_id: OwnedRoomId, limit: u32, reply: Reply },
    SendMessage { room_id: OwnedRoomId, body: String, reply: Reply },
    Profile { reply: Reply },
    Members { room_id: OwnedRoomId, limit: u32, reply: Reply },
    PinnedEvents { room_id: OwnedRoomId, reply: Reply },
    Threads { room_id: OwnedRoomId, limit: u32, reply: Reply },
    RoomsList { reply: Reply },
    Search { rooms: SearchRooms, query: String, limit: u32, server: bool, reply: Reply },
    /// Start or stop the worker's watch on a room for the incoming hooks.
    WatchRoom { room_id: OwnedRoomId },
    UnwatchRoom { room_id: OwnedRoomId },
    /// Same for the account-wide hooks (rooms list, invites, unread totals).
    WatchAccount { watch: bool },
    /// Sends an app bundle into a room as an `rs.robius.a2app` event.
    /// No reply: outcome is reported via a popup notification.
    ShareApp { room_id: OwnedRoomId, bundle_json: String, app_name: String },

    // --- room ---
    ThreadReplies { room_id: OwnedRoomId, event_id: OwnedEventId, limit: u32, reply: Reply },
    OlderMessages { room_id: OwnedRoomId, before: Option<OwnedEventId>, limit: u32, reply: Reply },
    Event { room_id: OwnedRoomId, event_id: OwnedEventId, reply: Reply },
    ReadReceipts { room_id: OwnedRoomId, user_id: Option<OwnedUserId>, reply: Reply },
    Unread { room_id: OwnedRoomId, reply: Reply },
    PowerLevels { room_id: OwnedRoomId, reply: Reply },
    Permalink { room_id: OwnedRoomId, event_id: Option<OwnedEventId>, use_matrix_scheme: bool, reply: Reply },
    Successor { room_id: OwnedRoomId, reply: Reply },

    // --- rooms ---
    RoomsSearch { query: String, limit: u32, reply: Reply },
    Invites { reply: Reply },
    RoomPreview { room: OwnedRoomOrAliasId, via: Vec<OwnedServerName>, reply: Reply },
    RoomsInfo { room_id: OwnedRoomId, reply: Reply },
    RoomsMessages { room_id: OwnedRoomId, limit: u32, reply: Reply },

    // --- spaces ---
    Spaces { reply: Reply },
    SpaceInfo { space_id: OwnedRoomId, reply: Reply },
    SpaceRooms { space_id: OwnedRoomId, reply: Reply },

    // --- account ---
    UserProfile { user_id: OwnedUserId, reply: Reply },
    DmFind { user_id: OwnedUserId, reply: Reply },
    Device { reply: Reply },
    AccountInfo { reply: Reply },
    IgnoredUsers { reply: Reply },

    // --- send ---
    Reply { room_id: OwnedRoomId, event_id: OwnedEventId, body: String, in_thread: bool, reply: Reply },
    React { room_id: OwnedRoomId, event_id: OwnedEventId, key: String, reply: Reply },
    Typing { room_id: OwnedRoomId, typing: bool, reply: Reply },
    ReadReceipt { room_id: OwnedRoomId, event_id: Option<OwnedEventId>, reply: Reply },
    Pin { room_id: OwnedRoomId, event_id: OwnedEventId, pinned: bool, reply: Reply },
    RoomFlag { room_id: OwnedRoomId, flag: RoomFlag, on: bool, reply: Reply },

    // --- membership ---
    Invite { room_id: OwnedRoomId, user_id: OwnedUserId, reply: Reply },
    Join { room: OwnedRoomOrAliasId, via: Vec<OwnedServerName>, reply: Reply },
    InviteRespond { room_id: OwnedRoomId, accept: bool, reply: Reply },
    DmOpen { user_id: OwnedUserId, reply: Reply },
}

/// The rooms a search covers.
#[derive(Debug)]
pub enum SearchRooms {
    One(OwnedRoomId),
    AllJoined,
    Some(Vec<OwnedRoomId>),
}

/// A finished matrix service call, posted back to the UI thread so the
/// result can re-enter the requesting isolate.
#[derive(Debug)]
pub struct A2AppMatrixResult {
    pub reply: Reply,
    pub result: Result<String, String>,
}

/// Maps a validated broker call onto the worker request that runs it.
/// `room` is the instance's attached room, already parsed.
pub fn request_for(
    call: MatrixServiceCall,
    room: Option<OwnedRoomId>,
    reply: Reply,
) -> Result<A2AppMatrixRequest, &'static str> {
    Ok(match (call, room) {
        (MatrixServiceCall::Profile, _) => A2AppMatrixRequest::Profile { reply },
        (MatrixServiceCall::RoomsList, _) => A2AppMatrixRequest::RoomsList { reply },
        (MatrixServiceCall::Search { query, scope: SearchScope::AllJoined, limit, server }, _) =>
            A2AppMatrixRequest::Search { rooms: SearchRooms::AllJoined, query, limit, server, reply },
        (MatrixServiceCall::Search { query, scope: SearchScope::Rooms(ids), limit, server }, _) => {
            let ids: Vec<OwnedRoomId> = ids.iter()
                .filter_map(|id| OwnedRoomId::try_from(id.as_str()).ok())
                .collect();
            if ids.is_empty() {
                return Err("no valid room ids");
            }
            A2AppMatrixRequest::Search { rooms: SearchRooms::Some(ids), query, limit, server, reply }
        }
        (MatrixServiceCall::Search { query, scope: SearchScope::Attached, limit, server }, Some(room_id)) =>
            A2AppMatrixRequest::Search { rooms: SearchRooms::One(room_id), query, limit, server, reply },
        // The room-free rooms, spaces and account services sit above the
        // no-room guard on purpose; `parse` has already checked the id sigils.
        (MatrixServiceCall::RoomsSearch { query, limit }, _) =>
            A2AppMatrixRequest::RoomsSearch { query, limit, reply },
        (MatrixServiceCall::Invites, _) => A2AppMatrixRequest::Invites { reply },
        (MatrixServiceCall::RoomPreview { room, via }, _) => {
            let room = OwnedRoomOrAliasId::try_from(room.as_str()).map_err(|_| "invalid room id or alias")?;
            let via = via.iter()
                .filter_map(|s| OwnedServerName::try_from(s.as_str()).ok())
                .collect();
            A2AppMatrixRequest::RoomPreview { room, via, reply }
        }
        (MatrixServiceCall::RoomsInfo { room_id }, _) => A2AppMatrixRequest::RoomsInfo {
            room_id: OwnedRoomId::try_from(room_id.as_str()).map_err(|_| "invalid room id")?,
            reply,
        },
        (MatrixServiceCall::RoomsMessages { room_id, limit }, _) => A2AppMatrixRequest::RoomsMessages {
            room_id: OwnedRoomId::try_from(room_id.as_str()).map_err(|_| "invalid room id")?,
            limit,
            reply,
        },
        (MatrixServiceCall::Spaces, _) => A2AppMatrixRequest::Spaces { reply },
        (MatrixServiceCall::SpaceInfo { space_id }, _) => A2AppMatrixRequest::SpaceInfo {
            space_id: OwnedRoomId::try_from(space_id.as_str()).map_err(|_| "invalid space id")?,
            reply,
        },
        (MatrixServiceCall::SpaceRooms { space_id }, _) => A2AppMatrixRequest::SpaceRooms {
            space_id: OwnedRoomId::try_from(space_id.as_str()).map_err(|_| "invalid space id")?,
            reply,
        },
        (MatrixServiceCall::UserProfile { user_id }, _) => A2AppMatrixRequest::UserProfile {
            user_id: OwnedUserId::try_from(user_id.as_str()).map_err(|_| "invalid user id")?,
            reply,
        },
        (MatrixServiceCall::DmFind { user_id }, _) => A2AppMatrixRequest::DmFind {
            user_id: OwnedUserId::try_from(user_id.as_str()).map_err(|_| "invalid user id")?,
            reply,
        },
        (MatrixServiceCall::Device, _) => A2AppMatrixRequest::Device { reply },
        (MatrixServiceCall::AccountInfo, _) => A2AppMatrixRequest::AccountInfo { reply },
        (MatrixServiceCall::IgnoredUsers, _) => A2AppMatrixRequest::IgnoredUsers { reply },
        (MatrixServiceCall::RoomInfo, Some(room_id)) =>
            A2AppMatrixRequest::RoomInfo { room_id, reply },
        (MatrixServiceCall::ReadMessages { limit }, Some(room_id)) =>
            A2AppMatrixRequest::ReadMessages { room_id, limit, reply },
        (MatrixServiceCall::SendMessage { body }, Some(room_id)) =>
            A2AppMatrixRequest::SendMessage { room_id, body, reply },
        (MatrixServiceCall::Members { limit }, Some(room_id)) =>
            A2AppMatrixRequest::Members { room_id, limit, reply },
        (MatrixServiceCall::PinnedEvents, Some(room_id)) =>
            A2AppMatrixRequest::PinnedEvents { room_id, reply },
        (MatrixServiceCall::Threads { limit }, Some(room_id)) =>
            A2AppMatrixRequest::Threads { room_id, limit, reply },

        // --- room ---
        (MatrixServiceCall::ThreadReplies { event_id, limit }, Some(room_id)) => {
            let Ok(event_id) = OwnedEventId::try_from(event_id.as_str()) else {
                return Err("not a valid event id");
            };
            A2AppMatrixRequest::ThreadReplies { room_id, event_id, limit, reply }
        }
        (MatrixServiceCall::OlderMessages { before, limit }, Some(room_id)) => {
            let Ok(before) = before.map(|id| OwnedEventId::try_from(id.as_str())).transpose() else {
                return Err("not a valid event id");
            };
            A2AppMatrixRequest::OlderMessages { room_id, before, limit, reply }
        }
        (MatrixServiceCall::Event { event_id }, Some(room_id)) => {
            let Ok(event_id) = OwnedEventId::try_from(event_id.as_str()) else {
                return Err("not a valid event id");
            };
            A2AppMatrixRequest::Event { room_id, event_id, reply }
        }
        (MatrixServiceCall::ReadReceipts { user_id }, Some(room_id)) => {
            let Ok(user_id) = user_id.map(|id| OwnedUserId::try_from(id.as_str())).transpose() else {
                return Err("not a valid user id");
            };
            A2AppMatrixRequest::ReadReceipts { room_id, user_id, reply }
        }
        (MatrixServiceCall::Unread, Some(room_id)) =>
            A2AppMatrixRequest::Unread { room_id, reply },
        (MatrixServiceCall::PowerLevels, Some(room_id)) =>
            A2AppMatrixRequest::PowerLevels { room_id, reply },
        (MatrixServiceCall::Permalink { event_id, use_matrix_scheme }, Some(room_id)) => {
            let Ok(event_id) = event_id.map(|id| OwnedEventId::try_from(id.as_str())).transpose() else {
                return Err("not a valid event id");
            };
            A2AppMatrixRequest::Permalink { room_id, event_id, use_matrix_scheme, reply }
        }
        (MatrixServiceCall::Successor, Some(room_id)) =>
            A2AppMatrixRequest::Successor { room_id, reply },

        // --- rooms ---

        // --- spaces ---

        // --- account ---

        // --- send ---
        (MatrixServiceCall::Reply { event_id, body, in_thread }, Some(room_id)) => A2AppMatrixRequest::Reply {
            room_id,
            event_id: OwnedEventId::try_from(event_id).map_err(|_| "invalid event_id")?,
            body,
            in_thread,
            reply,
        },
        (MatrixServiceCall::React { event_id, key }, Some(room_id)) => A2AppMatrixRequest::React {
            room_id,
            event_id: OwnedEventId::try_from(event_id).map_err(|_| "invalid event_id")?,
            key,
            reply,
        },
        (MatrixServiceCall::Typing { typing }, Some(room_id)) =>
            A2AppMatrixRequest::Typing { room_id, typing, reply },
        (MatrixServiceCall::ReadReceipt { event_id }, Some(room_id)) => A2AppMatrixRequest::ReadReceipt {
            room_id,
            event_id: event_id.map(OwnedEventId::try_from).transpose().map_err(|_| "invalid event_id")?,
            reply,
        },
        (MatrixServiceCall::Pin { event_id, pinned }, Some(room_id)) => A2AppMatrixRequest::Pin {
            room_id,
            event_id: OwnedEventId::try_from(event_id).map_err(|_| "invalid event_id")?,
            pinned,
            reply,
        },
        (MatrixServiceCall::RoomFlag { flag, on }, Some(room_id)) =>
            A2AppMatrixRequest::RoomFlag { room_id, flag, on, reply },
        (MatrixServiceCall::RoomsSend { room_id, body }, _) => A2AppMatrixRequest::SendMessage {
            room_id: OwnedRoomId::try_from(room_id).map_err(|_| "invalid room_id")?,
            body,
            reply,
        },

        // --- membership ---
        (MatrixServiceCall::Invite { user_id }, Some(room_id)) => A2AppMatrixRequest::Invite {
            room_id,
            user_id: OwnedUserId::try_from(user_id).map_err(|_| "invalid user_id")?,
            reply,
        },
        (MatrixServiceCall::Join { room, via }, _) => A2AppMatrixRequest::Join {
            room: OwnedRoomOrAliasId::try_from(room).map_err(|_| "invalid room id or alias")?,
            via: via.into_iter()
                .map(OwnedServerName::try_from)
                .collect::<Result<_, _>>()
                .map_err(|_| "invalid via server name")?,
            reply,
        },
        (MatrixServiceCall::InviteRespond { room_id, accept }, _) => A2AppMatrixRequest::InviteRespond {
            room_id: OwnedRoomId::try_from(room_id).map_err(|_| "invalid room_id")?,
            accept,
            reply,
        },
        (MatrixServiceCall::DmOpen { user_id }, _) => A2AppMatrixRequest::DmOpen {
            user_id: OwnedUserId::try_from(user_id).map_err(|_| "invalid user_id")?,
            reply,
        },

        (_, None) => {
            return Err("this mini-app is not attached to a room");
        }
    })
}

/// Cuts a body to `max` chars on a char boundary; a byte truncate could
/// split a multi-byte char and panic.
pub(crate) fn clip_chars(s: &mut String, max: usize) {
    if let Some((idx, _)) = s.char_indices().nth(max) {
        s.truncate(idx);
    }
}

/// Runs one mini-app matrix operation on the worker's async runtime and
/// posts the result back to the UI thread.
pub async fn handle_matrix_request(request: A2AppMatrixRequest) {
    use crate::sliding_sync::{current_user_id, get_client};

    let (reply, result) = match request {
        A2AppMatrixRequest::RoomInfo { room_id, reply } => (reply, room::info(room_id).await),
        A2AppMatrixRequest::ReadMessages { room_id, limit, reply } =>
            (reply, room::read_messages(room_id, limit, false).await),
        A2AppMatrixRequest::SendMessage { room_id, body, reply } =>
            (reply, send::message(room_id, body).await),
        A2AppMatrixRequest::Members { room_id, limit, reply } => {
            let result: Result<String, String> = async {
                use matrix_sdk::RoomMemberships;
                let client = get_client().ok_or("not logged in")?;
                let room = client.get_room(&room_id).ok_or("room not found")?;
                // A full /members sync on a big room can take tens of
                // seconds, so serve from the store unless it's too sparse
                // to even fill the requested page.
                let joined_count = room.joined_members_count();
                let mut members = room.members_no_sync(RoomMemberships::JOIN).await
                    .unwrap_or_default();
                if (members.len() as u64) < joined_count.min(limit as u64) {
                    members = room.members(RoomMemberships::JOIN).await
                        .map_err(|e| format!("couldn't load members: {e}"))?;
                }
                let count = (members.len() as u64).max(joined_count);
                let out: Vec<serde_json::Value> = members.iter()
                    .take(limit as usize)
                    .map(|m| {
                        use matrix_sdk::ruma::events::room::power_levels::UserPowerLevel;
                        // A room creator's power is "infinite" from room v12 on.
                        let power: i64 = match m.power_level() {
                            UserPowerLevel::Int(int) => int.into(),
                            _ => i64::MAX,
                        };
                        serde_json::json!({
                            "name": m.name(),
                            "user_id": m.user_id().to_string(),
                            "power": power,
                        })
                    })
                    .collect();
                Ok(serde_json::json!({ "count": count, "members": out }).to_string())
            }.await;
            (reply, result)
        }
        A2AppMatrixRequest::PinnedEvents { room_id, reply } => {
            let result: Result<String, String> = async {
                use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent};
                let client = get_client().ok_or("not logged in")?;
                let room = client.get_room(&room_id).ok_or("room not found")?;
                let pinned_ids = room.pinned_event_ids().unwrap_or_default();
                let mut out: Vec<serde_json::Value> = Vec::new();
                // Serve each pin from the event cache/store; only misses go
                // to the network, and those run concurrently.
                let cache = client.event_cache().room(&room_id).await.ok();
                let cache_ref = cache.as_ref().map(|(c, _)| c);
                let room_ref = &room;
                let fetched = futures_util::future::join_all(
                    pinned_ids.iter().take(10).map(|event_id| async move {
                        if let Some(c) = cache_ref {
                            if let Ok(Some(event)) = c.find_event(event_id).await {
                                return Some(event);
                            }
                        }
                        room_ref.event(event_id, None).await.ok()
                    })
                ).await;
                for event in fetched.into_iter().flatten() {
                    let Ok(AnySyncTimelineEvent::MessageLike(
                        AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(msg))
                    )) = event.raw().deserialize() else { continue };
                    let mut body = msg.content.body().to_string();
                    clip_chars(&mut body, 300);
                    out.push(serde_json::json!({
                        "sender": msg.sender.localpart(),
                        "sender_id": msg.sender,
                        "event_id": msg.event_id,
                        "body": body,
                    }));
                }
                Ok(serde_json::json!({ "pinned": out }).to_string())
            }.await;
            (reply, result)
        }
        A2AppMatrixRequest::Threads { room_id, limit, reply } => {
            let result: Result<String, String> = async {
                use matrix_sdk::room::ListThreadsOptions;
                use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent};
                let client = get_client().ok_or("not logged in")?;
                let room = client.get_room(&room_id).ok_or("room not found")?;
                let opts = ListThreadsOptions::default();
                let roots = room.list_threads(opts).await
                    .map_err(|e| format!("couldn't list threads: {e}"))?;
                let mut out: Vec<serde_json::Value> = Vec::new();
                for event in roots.chunk.iter().take(limit as usize) {
                    let Ok(AnySyncTimelineEvent::MessageLike(
                        AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(msg))
                    )) = event.raw().deserialize() else { continue };
                    let mut body = msg.content.body().to_string();
                    clip_chars(&mut body, 300);
                    out.push(serde_json::json!({
                        "sender": msg.sender.localpart(),
                        "sender_id": msg.sender,
                        "event_id": msg.event_id,
                        "body": body,
                    }));
                }
                Ok(serde_json::json!({ "threads": out }).to_string())
            }.await;
            (reply, result)
        }
        A2AppMatrixRequest::RoomsList { reply } => {
            let result: Result<String, String> = async {
                let client = get_client().ok_or("not logged in")?;
                let mut out: Vec<serde_json::Value> = Vec::new();
                for room in client.joined_rooms() {
                    out.push(serde_json::json!({
                        "room_id": room.room_id(),
                        "name": rooms::room_name(&room).await,
                        "is_direct": room.is_direct().await.unwrap_or(false),
                        "is_space": room.is_space(),
                        "member_count": room.joined_members_count(),
                        "is_encrypted": room.encryption_state().is_encrypted(),
                        "unread": room.num_unread_messages(),
                        "mentions": room.num_unread_mentions(),
                    }));
                }
                Ok(serde_json::json!({ "rooms": out }).to_string())
            }.await;
            (reply, result)
        }
        A2AppMatrixRequest::Search { rooms, query, limit, server, reply } => {
            use matrix_sdk::ruma::events::room::message::sanitize::remove_plain_reply_fallback;
            let result: Result<String, String> = async {
                use matrix_sdk::ruma::events::{AnyMessageLikeEvent, AnySyncMessageLikeEvent, AnySyncTimelineEvent, AnyTimelineEvent, MessageLikeEvent, SyncMessageLikeEvent};
                let client = get_client().ok_or("not logged in")?;
                let targets: Vec<matrix_sdk::Room> = match rooms {
                    SearchRooms::One(id) => vec![client.get_room(&id).ok_or("room not found")?],
                    SearchRooms::AllJoined => client.joined_rooms().into_iter().filter(|r| !r.is_space()).collect(),
                    SearchRooms::Some(ids) => ids.iter()
                        .filter_map(|id| client.get_room(id))
                        .filter(|r| r.state() == RoomState::Joined && !r.is_space())
                        .collect(),
                };
                let needle = query.to_lowercase();
                let mut seen: HashSet<OwnedEventId> = HashSet::new();
                let mut hits: Vec<(u64, serde_json::Value)> = Vec::new();
                for room in &targets {
                    let room_name = room.cached_display_name()
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| room.room_id().to_string());
                    // What is in memory plus everything Robrix has stored
                    // for the room: decrypted content, no network.
                    let mut events = Vec::new();
                    if let Ok((cache, _guard)) = client.event_cache().room(room.room_id()).await
                        && let Ok(cached) = cache.events().await
                    {
                        events.extend(cached);
                    }
                    if let Ok(store) = client.event_cache_store().lock().await
                        && let Some(store) = store.as_clean()
                        && let Ok(stored) = store.get_room_events(room.room_id(), Some("m.room.message"), None).await
                    {
                        events.extend(stored);
                    }
                    for event in events {
                        let Ok(AnySyncTimelineEvent::MessageLike(
                            AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(msg))
                        )) = event.raw().deserialize() else { continue };
                        if !seen.insert(msg.event_id.clone()) {
                            continue;
                        }
                        let mut body = remove_plain_reply_fallback(msg.content.body()).to_string();
                        if !body.to_lowercase().contains(&needle) {
                            continue;
                        }
                        clip_chars(&mut body, 300);
                        let ts = u64::from(msg.origin_server_ts.0);
                        hits.push((ts, serde_json::json!({
                            "room_id": room.room_id(),
                            "room_name": room_name,
                            "event_id": msg.event_id,
                            "sender": msg.sender.localpart(),
                            "sender_id": msg.sender,
                            "body": body,
                            "ts": ts,
                            "source": "local",
                        })));
                    }
                }
                let mut server_used = false;
                let unencrypted: Vec<OwnedRoomId> = targets.iter()
                    .filter(|r| !r.encryption_state().is_encrypted())
                    .map(|r| r.room_id().to_owned())
                    .collect();
                if server && !unencrypted.is_empty() {
                    use matrix_sdk::ruma::api::client::filter::RoomEventFilter;
                    use matrix_sdk::ruma::api::client::search::search_events::v3::{Categories, Criteria, OrderBy, Request, SearchKeys};
                    let mut criteria = Criteria::new(query.clone());
                    criteria.keys = Some(vec![SearchKeys::ContentBody]);
                    criteria.order_by = Some(OrderBy::Recent);
                    let mut filter = RoomEventFilter::default();
                    filter.rooms = Some(unencrypted);
                    criteria.filter = filter;
                    let mut categories = Categories::new();
                    categories.room_events = Some(criteria);
                    let response = client.send(Request::new(categories)).await
                        .map_err(|e| format!("server search failed: {e}"))?;
                    server_used = true;
                    for hit in response.search_categories.room_events.results {
                        let Some(raw) = hit.result else { continue };
                        let Ok(AnyTimelineEvent::MessageLike(
                            AnyMessageLikeEvent::RoomMessage(MessageLikeEvent::Original(msg))
                        )) = raw.deserialize() else { continue };
                        if !seen.insert(msg.event_id.clone()) {
                            continue;
                        }
                        let room_name = client.get_room(&msg.room_id)
                            .and_then(|r| r.cached_display_name())
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| msg.room_id.to_string());
                        let mut body = remove_plain_reply_fallback(msg.content.body()).to_string();
                        clip_chars(&mut body, 300);
                        let ts = u64::from(msg.origin_server_ts.0);
                        hits.push((ts, serde_json::json!({
                            "room_id": msg.room_id,
                            "room_name": room_name,
                            "event_id": msg.event_id,
                            "sender": msg.sender.localpart(),
                            "sender_id": msg.sender,
                            "body": body,
                            "ts": ts,
                            "source": "server",
                        })));
                    }
                }
                hits.sort_by_key(|(ts, _)| std::cmp::Reverse(*ts));
                hits.truncate(limit as usize);
                let results: Vec<serde_json::Value> = hits.into_iter().map(|(_, v)| v).collect();
                Ok(serde_json::json!({
                    "results": results,
                    "searched_rooms": targets.len(),
                    "server_used": server_used,
                }).to_string())
            }.await;
            (reply, result)
        }
        A2AppMatrixRequest::WatchRoom { room_id } => {
            room_watch::start_watch(room_id);
            return;
        }
        A2AppMatrixRequest::UnwatchRoom { room_id } => {
            room_watch::stop_watch(&room_id);
            return;
        }
        A2AppMatrixRequest::WatchAccount { watch } => {
            if watch { account_watch::start_watch() } else { account_watch::stop_watch() }
            return;
        }
        A2AppMatrixRequest::Profile { reply } => {
            let result: Result<String, String> = async {
                let client = get_client().ok_or("not logged in")?;
                let user_id = current_user_id().ok_or("not logged in")?;
                let display_name = client.account().get_display_name().await
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| user_id.localpart().to_string());
                Ok(serde_json::json!({
                    "user_id": user_id.to_string(),
                    "display_name": display_name,
                }).to_string())
            }.await;
            (reply, result)
        }
        A2AppMatrixRequest::ShareApp { room_id, bundle_json, app_name } => {
            let result: Result<(), String> = async {
                let client = get_client().ok_or("not logged in")?;
                let room = client.get_room(&room_id).ok_or("room not found")?;
                let content = serde_json::json!({
                    "body": format!("Shared a Splash mini-app: {app_name}"),
                    "bundle": bundle_json,
                });
                let raw = serde_json::value::to_raw_value(&content)
                    .map_err(|e| e.to_string())?;
                room.send_raw(crate::a2app::timeline_card::A2APP_EVENT_TYPE, raw).await
                    .map_err(|e| format!("couldn't share the app: {e}"))?;
                Ok(())
            }.await;
            match result {
                Ok(()) => enqueue_popup_notification(
                    format!("Shared \"{app_name}\" into the room."),
                    PopupKind::Success, Some(4.0),
                ),
                Err(e) => enqueue_popup_notification(e, PopupKind::Error, Some(5.0)),
            }
            return;
        }

        // --- room ---
        A2AppMatrixRequest::ThreadReplies { room_id, event_id, limit, reply } =>
            (reply, room::thread_replies(room_id, event_id, limit).await),
        A2AppMatrixRequest::OlderMessages { room_id, before, limit, reply } =>
            (reply, room::older_messages(room_id, before, limit, false).await),
        A2AppMatrixRequest::Event { room_id, event_id, reply } =>
            (reply, room::event(room_id, event_id).await),
        A2AppMatrixRequest::ReadReceipts { room_id, user_id, reply } =>
            (reply, room::read_receipts(room_id, user_id).await),
        A2AppMatrixRequest::Unread { room_id, reply } =>
            (reply, room::unread(room_id).await),
        A2AppMatrixRequest::PowerLevels { room_id, reply } =>
            (reply, room::power_levels(room_id).await),
        A2AppMatrixRequest::Permalink { room_id, event_id, use_matrix_scheme, reply } =>
            (reply, room::permalink(room_id, event_id, use_matrix_scheme).await),
        A2AppMatrixRequest::Successor { room_id, reply } =>
            (reply, room::successor(room_id).await),

        // --- rooms ---
        A2AppMatrixRequest::RoomsSearch { query, limit, reply } => (reply, rooms::search(query, limit).await),
        A2AppMatrixRequest::Invites { reply } => (reply, rooms::invites().await),
        A2AppMatrixRequest::RoomPreview { room, via, reply } => (reply, rooms::preview(room, via).await),
        A2AppMatrixRequest::RoomsInfo { room_id, reply } => (reply, room::info(room_id).await),
        A2AppMatrixRequest::RoomsMessages { room_id, limit, reply } =>
            (reply, room::read_messages(room_id, limit, false).await),

        // --- spaces ---
        A2AppMatrixRequest::Spaces { reply } => (reply, spaces::list().await),
        A2AppMatrixRequest::SpaceInfo { space_id, reply } => (reply, spaces::info(space_id).await),
        A2AppMatrixRequest::SpaceRooms { space_id, reply } => (reply, spaces::rooms(space_id).await),

        // --- account ---
        A2AppMatrixRequest::UserProfile { user_id, reply } => (reply, account::user_profile(user_id).await),
        A2AppMatrixRequest::DmFind { user_id, reply } => (reply, account::dm_find(user_id).await),
        A2AppMatrixRequest::Device { reply } => (reply, account::device().await),
        A2AppMatrixRequest::AccountInfo { reply } => (reply, account::info().await),
        A2AppMatrixRequest::IgnoredUsers { reply } => (reply, account::ignored_users().await),

        // --- send ---
        A2AppMatrixRequest::Reply { room_id, event_id, body, in_thread, reply } =>
            (reply, send::reply(room_id, event_id, body, in_thread).await),
        A2AppMatrixRequest::React { room_id, event_id, key, reply } =>
            (reply, send::react(room_id, event_id, key).await),
        A2AppMatrixRequest::Typing { room_id, typing, reply } =>
            (reply, send::typing(room_id, typing).await),
        A2AppMatrixRequest::ReadReceipt { room_id, event_id, reply } =>
            (reply, send::read_receipt(room_id, event_id).await),
        A2AppMatrixRequest::Pin { room_id, event_id, pinned, reply } =>
            (reply, send::pin(room_id, event_id, pinned).await),
        A2AppMatrixRequest::RoomFlag { room_id, flag, on, reply } =>
            (reply, send::room_flag(room_id, flag, on).await),

        // --- membership ---
        A2AppMatrixRequest::Invite { room_id, user_id, reply } =>
            (reply, membership::invite(room_id, user_id).await),
        A2AppMatrixRequest::Join { room, via, reply } =>
            (reply, membership::join(room, via).await),
        A2AppMatrixRequest::InviteRespond { room_id, accept, reply } =>
            (reply, membership::invite_respond(room_id, accept).await),
        A2AppMatrixRequest::DmOpen { user_id, reply } =>
            (reply, membership::dm_open(user_id).await),
    };
    Cx::post_action(A2AppMatrixResult { reply, result });
    SignalToUI::set_ui_signal();
}
