//! The `matrix.*` services: parsing and pricing live here (SDK-free); the
//! host runs the parsed [`MatrixServiceCall`] against the SDK and answers.
//!
//! Sections are per domain so the room, rooms, account and send services
//! can grow independently. A new service needs: its wire id in
//! [`is_service`], an arm in [`parse`], a variant here, and a price in
//! [`cost`].

/// A matrix.* service call, parsed and validated by the broker.
pub enum MatrixServiceCall {
    /// `{room_id, room_name, topic, member_count}` for the attached room.
    RoomInfo,
    /// The latest `limit` text messages, oldest first (limit already 1..=30).
    ReadMessages { limit: u32 },
    /// Read and decrypt one attachment from an event in the attached room.
    /// The host resolves its descriptor; the app cannot select an arbitrary URI.
    /// `preview` is `Some(longest edge)` exactly for `matrix.media_preview`.
    DownloadMedia { event_id: String, preview: Option<u32> },
    /// Send a plain text message to the attached room as the user.
    SendMessage { body: String },
    /// Answer an A2UI message in the attached room: `content` (a JSON object)
    /// becomes an `io.element.a2ui.action` event referencing `event_id`.
    A2uiAction { event_id: String, content: String },
    /// Upload and post inline media to the attached room as the user.
    /// Upload/send grants are separate from local composer drafting.
    SendMedia { data_base64: String, filename: String, mime_type: String, caption: Option<String> },
    /// `{user_id, display_name}` — the user's own identity.
    Profile,
    /// `{count, members: [{name, user_id, power}]}` for the attached room.
    Members { limit: u32 },
    /// `{pinned: [{sender, body}]}` — the room's pinned messages.
    PinnedEvents,
    /// `{threads: [{sender, body}]}` — thread roots, newest first.
    Threads { limit: u32 },
    /// Every joined room, with name, kind and encryption flag.
    RoomsList,
    /// Text search over messages; `scope` says which rooms.
    Search { query: String, scope: SearchScope, limit: u32, server: bool },

    // --- room ---
    /// `{root, replies: [message]}` for one thread, oldest first.
    ThreadReplies { event_id: String, limit: u32 },
    /// One page of text messages before `before`, or before the recent window.
    OlderMessages { before: Option<String>, limit: u32 },
    /// One event by id, with its reactions and edit state.
    Event { event_id: String },
    /// The latest read position of one member, or of every joined member.
    ReadReceipts { user_id: Option<String> },
    /// `{unread, mentions, marked_unread}` for the attached room.
    Unread,
    /// The user's power level here and what it lets them do.
    PowerLevels,
    /// A matrix.to (default) or matrix: link to the room or one of its events.
    Permalink { event_id: Option<String>, use_matrix_scheme: bool },
    /// Where an upgraded (tombstoned) room continued.
    Successor,

    // --- rooms ---
    /// Joined and invited rooms whose name or alias contains `query`.
    RoomsSearch { query: String, limit: u32 },
    /// Pending invites, with who sent each.
    Invites,
    /// Public details of any room by id or alias; hits the homeserver.
    RoomPreview { room: String, via: Vec<String> },
    /// The `RoomInfo` shape for any joined room.
    RoomsInfo { room_id: String },
    /// The `ReadMessages` shape for any joined room (limit already 1..=30).
    RoomsMessages { room_id: String, limit: u32 },

    // --- spaces ---
    /// Every joined space.
    Spaces,
    /// One joined space's details.
    SpaceInfo { space_id: String },
    /// A space's direct child rooms and subspaces.
    SpaceRooms { space_id: String },

    // --- account ---
    /// Any user's public profile, plus whether the user ignores them.
    UserProfile { user_id: String },
    /// The existing DM room with a user, if any; never creates one.
    DmFind { user_id: String },
    /// This device's id, name and verification state.
    Device,
    /// User id, homeserver and account-management URL.
    AccountInfo,
    /// The account's ignore list.
    IgnoredUsers,
    /// This app's saved account data for `keys`, deduped in asked order. A `{keys}`
    /// read (`is_batch`) answers `{items: [{key, content}]}`, else `{key, content}`.
    AccountData { keys: Vec<String>, is_batch: bool },
    /// Save `content` under `key` in this app's account data; None deletes it.
    SetAccountData { key: String, content: Option<serde_json::Value> },
    /// This app's private notes for the attached room, shaped like `AccountData`.
    RoomAccountData { keys: Vec<String>, is_batch: bool },
    /// Save one of this app's notes for the attached room; None deletes it.
    SetRoomAccountData { key: String, content: Option<serde_json::Value> },

    // --- send ---
    /// Reply to `event_id`, or post into the thread rooted there when
    /// `in_thread`; answers `{event_id}` of the sent event.
    Reply { event_id: String, body: String, in_thread: bool },
    /// Toggle the user's reaction `key` on an event; answers `{added}`.
    React { event_id: String, key: String },
    /// Show or hide the user as typing.
    Typing { typing: bool },
    /// A read receipt up to `event_id`, or fully read when absent.
    ReadReceipt { event_id: Option<String> },
    /// Pin or unpin an event.
    Pin { event_id: String, pinned: bool },
    /// Turn one of the attached room's own flags on or off.
    RoomFlag { flag: RoomFlag, on: bool },
    /// Plain text to any joined room, `room_id` as given.
    RoomsSend { room_id: String, body: String },

    // --- membership ---
    /// Invite `user_id` to the attached room.
    Invite { user_id: String },
    /// Join a room by id or alias, knocking when it is invite-only.
    Join { room: String, via: Vec<String> },
    /// Accept or decline a pending invite.
    InviteRespond { room_id: String, accept: bool },
    /// The DM with `user_id`, created when there is none.
    DmOpen { user_id: String },
}

/// Which rooms a `matrix.search_*` call covers.
pub enum SearchScope {
    Attached,
    AllJoined,
    /// Room ids as given; the host validates them.
    Rooms(Vec<String>),
}

/// The per-room flags `matrix.favorite`, `matrix.low_priority` and
/// `matrix.mark_unread` set.
#[derive(Debug, Clone, Copy)]
pub enum RoomFlag {
    Favorite,
    LowPriority,
    Unread,
}

/// Every wire id the broker routes here.
pub fn is_service(service: &str) -> bool {
    service.starts_with("matrix.")
}

/// Services that work without an attached room.
fn room_free(service: &str) -> bool {
    matches!(service,
        "matrix.profile" | "matrix.rooms_list" | "matrix.search_rooms"
        | "matrix.rooms_search" | "matrix.invites" | "matrix.room_preview"
        | "matrix.rooms_info" | "matrix.rooms_messages"
        | "matrix.spaces" | "matrix.space_info" | "matrix.space_rooms"
        | "matrix.user_profile" | "matrix.dm_find" | "matrix.device"
        | "matrix.account_info" | "matrix.ignored_users"
        | "matrix.account_data" | "matrix.set_account_data"

        | "matrix.rooms_send" | "matrix.join" | "matrix.invite_respond" | "matrix.dm_open"

    )
}

/// A Matrix id argument, checked only for its sigil; the host parses it.
fn id_arg(service: &str, args: &serde_json::Value, key: &str, sigil: char) -> Result<String, String> {
    let id = args[key].as_str().map(str::trim).unwrap_or_default();
    if !id.starts_with(sigil) {
        return Err(format!("{service} needs {{{key}}}"));
    }
    Ok(id.to_string())
}

/// A required string argument, trimmed and non-empty.
fn required_str(service: &str, args: &serde_json::Value, key: &str) -> Result<String, String> {
    match args[key].as_str().map(str::trim) {
        Some(s) if !s.is_empty() => Ok(s.to_string()),
        _ => Err(format!("{service} needs {{{key}}}")),
    }
}

fn required_bool(service: &str, args: &serde_json::Value, key: &str) -> Result<bool, String> {
    args[key].as_bool().ok_or_else(|| format!("{service} needs {{{key}: bool}}"))
}

/// A preview's longest edge: a whole number of pixels in range, 256 when absent.
fn preview_edge(size: Option<&serde_json::Value>) -> Result<u32, String> {
    use super::media_preview::{DEFAULT_EDGE, MAX_EDGE, MIN_EDGE};
    let Some(size) = size else { return Ok(DEFAULT_EDGE) };
    // Script numbers are doubles, so 256.0 is the whole number 256.
    size.as_f64().filter(|edge| edge.fract() == 0.0 && (f64::from(MIN_EDGE)..=f64::from(MAX_EDGE)).contains(edge))
        .map(|edge| edge as u32)
        .ok_or_else(|| format!("matrix.media_preview {{size}} must be a whole number of pixels from {MIN_EDGE} to {MAX_EDGE}"))
}

/// The one event an attachment read names: only `allowed` keys, and a valid `event_id`.
fn attachment_event(service: &str, args: &serde_json::Value, allowed: &[&str]) -> Result<String, String> {
    let object = args.as_object().ok_or_else(|| format!("{service} needs {{event_id}}"))?;
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!("{service} does not accept '{key}'; it reads {{{}}} from the attached room", allowed.join(", ")));
    }
    let event_id = id_arg(service, args, "event_id", '$')?;
    if event_id.len() == 1 || event_id.chars().any(char::is_whitespace) || event_id.chars().any(char::is_control) {
        return Err(format!("{service} needs a valid {{event_id}}"));
    }
    Ok(event_id)
}

/// A message body: `required_str` plus the cap every text send shares.
fn text_body(service: &str, args: &serde_json::Value) -> Result<String, String> {
    let body = required_str(service, args, "body")?;
    if body.chars().count() > 4096 {
        return Err("message is too long (4096 characters max)".into());
    }
    Ok(body)}

/// The most an A2UI action's content may take: events max out at 64 KiB, and
/// encryption grows content by about a third.
const MAX_A2UI_ACTION_BYTES: usize = 44 * 1024;

/// The arguments of an account-data service, refusing any but `allowed`:
/// Robrix chooses the event type, so `type`, `room_id` and the like never apply.
fn account_data_args<'a>(service: &str, args: &'a serde_json::Value, allowed: &[&str])
    -> Result<&'a serde_json::Map<String, serde_json::Value>, String>
{
    let shape = if allowed.contains(&"content") { "{key, content}" } else { "{key} or {keys}" };
    match args.as_object() {
        Some(object) if object.keys().all(|name| allowed.contains(&name.as_str())) => Ok(object),
        Some(_) => Err(format!("{service} takes only {shape}: Robrix chooses the event type.")),
        None => Err(format!("{service} needs {shape}")),
    }
}

/// A read's `{key}` or `{keys: [..]}`: valid keys, deduped in asked order, and
/// whether the app asked for a batch.
fn account_data_keys(service: &str, args: &serde_json::Value) -> Result<(Vec<String>, bool), String> {
    use crate::account_data::{MAX_KEYS, validate_key, INVALID_KEY};
    let object = account_data_args(service, args, &["key", "keys"])?;
    let parse_key = |value: &serde_json::Value| -> Result<String, String> {
        let key = value.as_str().ok_or(INVALID_KEY)?;
        validate_key(key)?;
        Ok(key.to_string())
    };
    match (object.get("key"), object.get("keys")) {
        (Some(single), None) => Ok((vec![parse_key(single)?], false)),
        (None, Some(many)) => {
            let many = many.as_array().filter(|many| (1..=MAX_KEYS).contains(&many.len()))
                .ok_or_else(|| format!("{service} {{keys}} must list 1 to {MAX_KEYS} keys"))?;
            let mut keys: Vec<String> = Vec::new();
            for value in many {
                let key = parse_key(value)?;
                if !keys.contains(&key) { keys.push(key); }
            }
            Ok((keys, true))
        }
        (Some(_), Some(_)) => Err(format!("{service} takes {{key}} or {{keys}}, not both")),
        (None, None) => Err(format!("{service} needs {{key}} or {{keys}}")),
    }
}

/// A write's `{key, content}`: an object within the size caps, or null to delete.
fn account_data_write(service: &str, args: &serde_json::Value) -> Result<(String, Option<serde_json::Value>), String> {
    use crate::account_data::{validate_content, validate_key, INVALID_KEY};
    let object = account_data_args(service, args, &["key", "content"])?;
    let (Some(key), Some(content)) = (object.get("key"), object.get("content")) else {
        return Err(format!("{service} needs {{key, content}}; content: null deletes the key"));
    };
    let key = key.as_str().ok_or(INVALID_KEY)?;
    validate_key(key)?;
    if content.is_null() { return Ok((key.to_string(), None)); }
    validate_content(key, content)?;
    Ok((key.to_string(), Some(content.clone())))
}

/// Validates and clamps a call's arguments. Runs AFTER the permission gate,
/// so a first-use prompt still reads sensibly before an argument error.
pub fn parse(service: &str, args: &serde_json::Value, has_room: bool) -> Result<MatrixServiceCall, String> {
    if !room_free(service) && !has_room {
        return Err("this mini-app is not attached to a room".into());
    }
    Ok(match service {
        "matrix.room_info" => MatrixServiceCall::RoomInfo,
        "matrix.profile" => MatrixServiceCall::Profile,
        "matrix.rooms_list" => MatrixServiceCall::RoomsList,
        "matrix.search_room" | "matrix.search_rooms" => {
            let query = args["query"].as_str().map(str::trim).unwrap_or_default();
            if query.is_empty() {
                return Err(format!("{service} needs {{query}}"));
            }
            let scope = if service == "matrix.search_room" {
                SearchScope::Attached
            } else {
                match args["room_ids"].as_array() {
                    Some(ids) => SearchScope::Rooms(
                        ids.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
                    ),
                    None => SearchScope::AllJoined,
                }
            };
            MatrixServiceCall::Search {
                query: query.to_string(),
                scope,
                limit: args["limit"].as_u64().unwrap_or(25) as u32,
                server: args["server"].as_bool().unwrap_or(false),
            }
        }
        "matrix.read_messages" => MatrixServiceCall::ReadMessages {
            limit: args["limit"].as_u64().unwrap_or(10).clamp(1, 30) as u32,
        },
        "matrix.download_media" => MatrixServiceCall::DownloadMedia {
            event_id: attachment_event(service, args, &["event_id"])?, preview: None,
        },
        "matrix.media_preview" => MatrixServiceCall::DownloadMedia {
            event_id: attachment_event(service, args, &["event_id", "size"])?,
            preview: Some(preview_edge(args.get("size"))?),
        },
        "matrix.room_members" => MatrixServiceCall::Members {
            limit: args["limit"].as_u64().unwrap_or(50).clamp(1, 200) as u32,
        },
        "matrix.pinned_events" => MatrixServiceCall::PinnedEvents,
        "matrix.room_threads" => MatrixServiceCall::Threads {
            limit: args["limit"].as_u64().unwrap_or(20).clamp(1, 50) as u32,
        },
        "matrix.send_message" => MatrixServiceCall::SendMessage { body: text_body(service, args)? },
        "matrix.a2ui_action" => {
            let event_id = args["event_id"].as_str().filter(|id| id.starts_with('$'))
                .ok_or("matrix.a2ui_action needs {event_id}")?;
            let content = args["content"].as_str()
                .filter(|content| content.len() <= MAX_A2UI_ACTION_BYTES)
                .filter(|content| serde_json::from_str::<serde_json::Value>(content).is_ok_and(|v| v.is_object()))
                .ok_or("matrix.a2ui_action needs {content}: a JSON object of at most 44 KiB")?;
            MatrixServiceCall::A2uiAction { event_id: event_id.into(), content: content.into() }
        }
        "matrix.send_media" => {
            if args.get("room_id").is_some() {
                return Err("matrix.send_media only posts to the attached room; room_id is not accepted".into());
            }
            let super::HostAction::ComposerAttach { data_base64, filename, mime_type, caption, .. } =
                super::parse_composer_attachment(args, None).map_err(|error| error.replace("composer.attach", service))?
                else { unreachable!("the inline parser only returns an attachment") };
            MatrixServiceCall::SendMedia { data_base64, filename, mime_type, caption }
        },

        // --- room ---
        "matrix.thread_replies" => {
            let Some(event_id) = args["event_id"].as_str().map(str::trim).filter(|s| !s.is_empty()) else {
                return Err("matrix.thread_replies needs {event_id}".into());
            };
            MatrixServiceCall::ThreadReplies {
                event_id: event_id.to_string(),
                limit: args["limit"].as_u64().unwrap_or(50).clamp(1, 100) as u32,
            }
        }
        "matrix.older_messages" => MatrixServiceCall::OlderMessages {
            before: args["before"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string),
            limit: args["limit"].as_u64().unwrap_or(20).clamp(1, 50) as u32,
        },
        "matrix.event" => {
            let Some(event_id) = args["event_id"].as_str().map(str::trim).filter(|s| !s.is_empty()) else {
                return Err("matrix.event needs {event_id}".into());
            };
            MatrixServiceCall::Event { event_id: event_id.to_string() }
        }
        "matrix.read_receipts" => MatrixServiceCall::ReadReceipts {
            user_id: args["user_id"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string),
        },
        "matrix.unread" => MatrixServiceCall::Unread,
        "matrix.power_levels" => MatrixServiceCall::PowerLevels,
        "matrix.permalink" => {
            let use_matrix_scheme = match args["scheme"].as_str().unwrap_or("matrix.to") {
                "matrix.to" => false,
                "matrix" => true,
                _ => return Err("scheme must be \"matrix.to\" or \"matrix\"".into()),
            };
            MatrixServiceCall::Permalink {
                event_id: args["event_id"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string),
                use_matrix_scheme,
            }
        }
        "matrix.successor" => MatrixServiceCall::Successor,

        // --- rooms ---
        "matrix.rooms_search" => {
            let query = args["query"].as_str().map(str::trim).unwrap_or_default();
            if query.is_empty() {
                return Err("matrix.rooms_search needs {query}".into());
            }
            MatrixServiceCall::RoomsSearch {
                query: query.to_string(),
                limit: args["limit"].as_u64().unwrap_or(20).clamp(1, 50) as u32,
            }
        }
        "matrix.invites" => MatrixServiceCall::Invites,
        "matrix.room_preview" => {
            let room = args["room"].as_str().map(str::trim).unwrap_or_default();
            if !room.starts_with(['!', '#']) {
                return Err("matrix.room_preview needs {room}, a room id or alias".into());
            }
            let via = args["via"].as_array()
                .map(|v| v.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            MatrixServiceCall::RoomPreview { room: room.to_string(), via }
        }
        "matrix.rooms_info" => MatrixServiceCall::RoomsInfo {
            room_id: id_arg(service, args, "room_id", '!')?,
        },
        "matrix.rooms_messages" => MatrixServiceCall::RoomsMessages {
            room_id: id_arg(service, args, "room_id", '!')?,
            limit: args["limit"].as_u64().unwrap_or(10).clamp(1, 30) as u32,
        },

        // --- spaces ---
        "matrix.spaces" => MatrixServiceCall::Spaces,
        "matrix.space_info" => MatrixServiceCall::SpaceInfo {
            space_id: id_arg(service, args, "space_id", '!')?,
        },
        "matrix.space_rooms" => MatrixServiceCall::SpaceRooms {
            space_id: id_arg(service, args, "space_id", '!')?,
        },

        // --- account ---
        "matrix.user_profile" => MatrixServiceCall::UserProfile {
            user_id: id_arg(service, args, "user_id", '@')?,
        },
        "matrix.dm_find" => MatrixServiceCall::DmFind {
            user_id: id_arg(service, args, "user_id", '@')?,
        },
        "matrix.device" => MatrixServiceCall::Device,
        "matrix.account_info" => MatrixServiceCall::AccountInfo,
        "matrix.ignored_users" => MatrixServiceCall::IgnoredUsers,
        "matrix.account_data" | "matrix.room_account_data" => {
            let (keys, is_batch) = account_data_keys(service, args)?;
            if service == "matrix.account_data" { MatrixServiceCall::AccountData { keys, is_batch } }
            else { MatrixServiceCall::RoomAccountData { keys, is_batch } }
        }
        "matrix.set_account_data" | "matrix.set_room_account_data" => {
            let (key, content) = account_data_write(service, args)?;
            if service == "matrix.set_account_data" { MatrixServiceCall::SetAccountData { key, content } }
            else { MatrixServiceCall::SetRoomAccountData { key, content } }
        }

        // --- send ---
        "matrix.reply" | "matrix.thread_reply" => MatrixServiceCall::Reply {
            event_id: required_str(service, args, "event_id")?,
            body: text_body(service, args)?,
            in_thread: service == "matrix.thread_reply",
        },
        "matrix.react" => {
            let key = required_str(service, args, "key")?;
            if key.chars().count() > 32 {
                return Err("reaction key is too long (32 characters max)".into());
            }
            MatrixServiceCall::React { event_id: required_str(service, args, "event_id")?, key }
        }
        "matrix.typing" => MatrixServiceCall::Typing { typing: required_bool(service, args, "typing")? },
        "matrix.read_receipt" => MatrixServiceCall::ReadReceipt {
            event_id: args["event_id"].as_str().map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        },
        "matrix.pin" => MatrixServiceCall::Pin {
            event_id: required_str(service, args, "event_id")?,
            pinned: required_bool(service, args, "pinned")?,
        },
        "matrix.favorite" | "matrix.low_priority" | "matrix.mark_unread" => MatrixServiceCall::RoomFlag {
            flag: match service {
                "matrix.favorite" => RoomFlag::Favorite,
                "matrix.low_priority" => RoomFlag::LowPriority,
                _ => RoomFlag::Unread,
            },
            on: required_bool(service, args, "on")?,
        },
        "matrix.rooms_send" => MatrixServiceCall::RoomsSend {
            room_id: required_str(service, args, "room_id")?,
            body: text_body(service, args)?,
        },

        // --- membership ---
        "matrix.invite" => MatrixServiceCall::Invite { user_id: required_str(service, args, "user_id")? },
        "matrix.join" => MatrixServiceCall::Join {
            room: required_str(service, args, "room")?,
            via: args["via"].as_array()
                .map(|v| v.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
        },
        "matrix.invite_respond" => MatrixServiceCall::InviteRespond {
            room_id: required_str(service, args, "room_id")?,
            accept: required_bool(service, args, "accept")?,
        },
        "matrix.dm_open" => MatrixServiceCall::DmOpen { user_id: required_str(service, args, "user_id")? },

        _ => return Err(format!("unknown service '{service}'")),
    })
}

/// Request-budget price of a matrix service, in the limiter's tokens.
pub fn cost(service: &str) -> f64 {
    match service {
        // ----- room: answered from state the host already holds -----
        "matrix.room_info" | "matrix.unread" | "matrix.power_levels" | "matrix.permalink"
        | "matrix.successor" => 1.0,
        // ----- room: a worker round-trip -----
        "matrix.read_messages" | "matrix.room_members" | "matrix.pinned_events"
        | "matrix.room_threads" | "matrix.search_room" | "matrix.thread_replies"
        | "matrix.older_messages" | "matrix.event" | "matrix.read_receipts" | "matrix.download_media" => 5.0,
        // A bounded small re-encode: capped source bytes and decode memory.
        "matrix.media_preview" => 2.0,
        // ----- rooms: fans out over every room and may hit the network -----
        "matrix.rooms_list" => 5.0,
        "matrix.search_rooms" => 8.0,
        "matrix.rooms_search" | "matrix.invites" => 5.0,
        "matrix.room_preview" | "matrix.rooms_messages" => 8.0,
        "matrix.rooms_info" => 1.0,
        // ----- account -----
        "matrix.profile" => 1.0,
        "matrix.device" | "matrix.account_info" | "matrix.ignored_users" | "matrix.dm_find" => 1.0,
        "matrix.user_profile" => 5.0,
        // One charge per read, however many keys; a write reaches the server.
        "matrix.account_data" | "matrix.room_account_data" => 1.0,
        "matrix.set_account_data" | "matrix.set_room_account_data" => 8.0,
        // ----- send: speaks as the user -----
        "matrix.send_message" | "matrix.a2ui_action" => 5.0,
        "matrix.send_media" => 8.0,
        "matrix.typing" => 1.0,
        "matrix.favorite" | "matrix.low_priority" | "matrix.mark_unread" => 5.0,
        "matrix.reply" | "matrix.thread_reply" | "matrix.react" | "matrix.read_receipt"
        | "matrix.pin" | "matrix.rooms_send" => 8.0,

        // --- spaces ---
        "matrix.spaces" => 5.0,
        "matrix.space_info" => 1.0,
        "matrix.space_rooms" => 8.0,

        // --- membership ---
        "matrix.invite" | "matrix.join" | "matrix.invite_respond" | "matrix.dm_open" => 8.0,

        _ => 8.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_attached(service: &str, args: serde_json::Value) -> Result<MatrixServiceCall, String> {
        parse(service, &args, true)
    }

    #[test]
    fn room_services_need_a_room() {
        for service in [
            "matrix.thread_replies", "matrix.older_messages", "matrix.event", "matrix.read_receipts",
            "matrix.unread", "matrix.power_levels", "matrix.permalink", "matrix.successor",
            "matrix.download_media", "matrix.media_preview",
            "matrix.room_account_data", "matrix.set_room_account_data",
        ] {
            let err = parse(service, &serde_json::json!({}), false).err();
            assert_eq!(err.as_deref(), Some("this mini-app is not attached to a room"), "{service}");
        }
    }

    #[test]
    fn thread_replies_needs_an_event_id_and_clamps_limit() {
        assert!(parse_attached("matrix.thread_replies", serde_json::json!({})).is_err());
        assert!(parse_attached("matrix.thread_replies", serde_json::json!({"event_id": "  "})).is_err());
        let MatrixServiceCall::ThreadReplies { event_id, limit } =
            parse_attached("matrix.thread_replies", serde_json::json!({"event_id": " $root ", "limit": 500})).unwrap()
        else { panic!("expected ThreadReplies") };
        assert_eq!(event_id, "$root");
        assert_eq!(limit, 100);
        let MatrixServiceCall::ThreadReplies { limit, .. } =
            parse_attached("matrix.thread_replies", serde_json::json!({"event_id": "$root"})).unwrap()
        else { panic!("expected ThreadReplies") };
        assert_eq!(limit, 50);
        let MatrixServiceCall::ThreadReplies { limit, .. } =
            parse_attached("matrix.thread_replies", serde_json::json!({"event_id": "$root", "limit": 0})).unwrap()
        else { panic!("expected ThreadReplies") };
        assert_eq!(limit, 1);
    }

    #[test]
    fn older_messages_takes_an_optional_anchor_and_clamps_limit() {
        let MatrixServiceCall::OlderMessages { before, limit } =
            parse_attached("matrix.older_messages", serde_json::json!({})).unwrap()
        else { panic!("expected OlderMessages") };
        assert!(before.is_none());
        assert_eq!(limit, 20);
        let MatrixServiceCall::OlderMessages { before, limit } =
            parse_attached("matrix.older_messages", serde_json::json!({"before": " $e ", "limit": 99})).unwrap()
        else { panic!("expected OlderMessages") };
        assert_eq!(before.as_deref(), Some("$e"));
        assert_eq!(limit, 50);
        let MatrixServiceCall::OlderMessages { before, limit } =
            parse_attached("matrix.older_messages", serde_json::json!({"before": "", "limit": 0})).unwrap()
        else { panic!("expected OlderMessages") };
        assert!(before.is_none());
        assert_eq!(limit, 1);
    }

    #[test]
    fn event_needs_an_event_id() {
        assert_eq!(
            parse_attached("matrix.event", serde_json::json!({})).err().as_deref(),
            Some("matrix.event needs {event_id}"),
        );
        let MatrixServiceCall::Event { event_id } =
            parse_attached("matrix.event", serde_json::json!({"event_id": "$e"})).unwrap()
        else { panic!("expected Event") };
        assert_eq!(event_id, "$e");
    }

    #[test]
    fn media_download_reads_one_valid_event_from_the_attached_room() {
        let args = serde_json::json!({"event_id": " $attachment:test "});
        let MatrixServiceCall::DownloadMedia { event_id, preview } = parse_attached("matrix.download_media", args.clone()).unwrap()
            else { panic!("expected an attachment read") };
        assert_eq!(event_id, "$attachment:test");
        assert_eq!(preview, None, "a download is always the whole file");
        assert_eq!(cost("matrix.download_media"), cost("matrix.event"));
        assert!(matches!(parse("matrix.download_media", &args, false), Err(error) if error.contains("not attached to a room")));
        for args in [serde_json::json!({}), serde_json::json!({"event_id": "$"}),
            serde_json::json!({"event_id": "mxc://server/media"}), serde_json::json!({"event_id": "$two words"}),
            serde_json::json!({"event_id": "$control\u{0}"}), serde_json::json!({"event_id": 3}), serde_json::json!("$a:test")]
        {
            assert!(parse_attached("matrix.download_media", args).is_err());
        }
        // The preview keys are unknown here, so a whole file is never fetched at the preview price.
        for key in ["room_id", "source_room", "uri", "url", "thumbnail", "filename", "preview", "size"] {
            let mut args = args.clone();
            args[key] = serde_json::json!(true);
            assert!(matches!(parse_attached("matrix.download_media", args), Err(error) if error.contains(&format!("'{key}'"))), "{key}");
        }
    }

    #[test]
    fn media_preview_takes_a_whole_pixel_edge_and_keeps_the_download_strictness() {
        let preview = |args: serde_json::Value| match parse_attached("matrix.media_preview", args) {
            Ok(MatrixServiceCall::DownloadMedia { event_id, preview }) => { assert_eq!(event_id, "$a:test"); Ok(preview) }
            Ok(_) => panic!("expected an attachment read"),
            Err(error) => Err(error),
        };
        assert_eq!(preview(serde_json::json!({"event_id": " $a:test "})), Ok(Some(256)), "a preview is never the whole file");
        for (size, edge) in [(serde_json::json!(64), 64), (serde_json::json!(300), 300), (serde_json::json!(1024), 1024),
            (serde_json::json!(256.0), 256)]
        {
            assert_eq!(preview(serde_json::json!({"event_id": "$a:test", "size": size})), Ok(Some(edge)));
        }
        for size in [serde_json::json!(63), serde_json::json!(1025), serde_json::json!(0), serde_json::json!(-256),
            serde_json::json!(256.5), serde_json::json!(1e300), serde_json::json!("256"), serde_json::json!(null), serde_json::json!(true)]
        {
            let error = preview(serde_json::json!({"event_id": "$a:test", "size": size})).unwrap_err();
            assert!(error.starts_with("matrix.media_preview {size}") && error.contains("64 to 1024"), "{size}: {error}");
        }
        for key in ["preview", "room_id", "source_room", "uri", "url", "thumbnail", "filename", "width", "mime_type"] {
            let args = serde_json::json!({"event_id": "$a:test", "size": 128, key: true});
            assert!(matches!(preview(args), Err(error) if error.starts_with("matrix.media_preview") && error.contains(&format!("'{key}'"))), "{key}");
        }
        for args in [serde_json::json!({"size": 128}), serde_json::json!({"event_id": "$"}), serde_json::json!({"event_id": "$two words"}),
            serde_json::json!({"event_id": "mxc://server/media"}), serde_json::json!(null)]
        {
            assert!(preview(args).is_err());
        }
        assert!(matches!(parse("matrix.media_preview", &serde_json::json!({"event_id": "$a:test"}), false),
            Err(error) if error.contains("not attached to a room")));
        assert!(cost("matrix.media_preview") < cost("matrix.download_media"), "a bounded preview is cheaper than a whole file");
    }

    #[test]
    fn read_receipts_user_is_optional() {
        let MatrixServiceCall::ReadReceipts { user_id } =
            parse_attached("matrix.read_receipts", serde_json::json!({})).unwrap()
        else { panic!("expected ReadReceipts") };
        assert!(user_id.is_none());
        let MatrixServiceCall::ReadReceipts { user_id } =
            parse_attached("matrix.read_receipts", serde_json::json!({"user_id": "@a:b"})).unwrap()
        else { panic!("expected ReadReceipts") };
        assert_eq!(user_id.as_deref(), Some("@a:b"));
    }

    #[test]
    fn permalink_scheme_defaults_to_matrix_to() {
        let MatrixServiceCall::Permalink { event_id, use_matrix_scheme } =
            parse_attached("matrix.permalink", serde_json::json!({})).unwrap()
        else { panic!("expected Permalink") };
        assert!(event_id.is_none());
        assert!(!use_matrix_scheme);
        let MatrixServiceCall::Permalink { event_id, use_matrix_scheme } =
            parse_attached("matrix.permalink", serde_json::json!({"event_id": "$e", "scheme": "matrix"})).unwrap()
        else { panic!("expected Permalink") };
        assert_eq!(event_id.as_deref(), Some("$e"));
        assert!(use_matrix_scheme);
        assert!(parse_attached("matrix.permalink", serde_json::json!({"scheme": "https"})).is_err());
    }

    #[test]
    fn argument_free_room_services_parse() {
        assert!(matches!(parse_attached("matrix.unread", serde_json::json!({})), Ok(MatrixServiceCall::Unread)));
        assert!(matches!(parse_attached("matrix.power_levels", serde_json::json!({})), Ok(MatrixServiceCall::PowerLevels)));
        assert!(matches!(parse_attached("matrix.successor", serde_json::json!({})), Ok(MatrixServiceCall::Successor)));
    }
}

#[cfg(test)]
mod rooms_and_account_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rooms_search_needs_a_query_and_clamps_limit() {
        assert!(parse("matrix.rooms_search", &json!({}), false).is_err());
        assert!(parse("matrix.rooms_search", &json!({"query": "  "}), false).is_err());
        let Ok(MatrixServiceCall::RoomsSearch { query, limit }) =
            parse("matrix.rooms_search", &json!({"query": " rust ", "limit": 500}), false) else { panic!() };
        assert_eq!((query.as_str(), limit), ("rust", 50));
        let Ok(MatrixServiceCall::RoomsSearch { limit, .. }) =
            parse("matrix.rooms_search", &json!({"query": "x", "limit": 0}), false) else { panic!() };
        assert_eq!(limit, 1);
        let Ok(MatrixServiceCall::RoomsSearch { limit, .. }) =
            parse("matrix.rooms_search", &json!({"query": "x"}), false) else { panic!() };
        assert_eq!(limit, 20);
    }

    #[test]
    fn invites_parses_without_a_room() {
        assert!(matches!(parse("matrix.invites", &json!({}), false), Ok(MatrixServiceCall::Invites)));
    }

    #[test]
    fn room_preview_takes_an_id_or_alias_and_via() {
        assert!(parse("matrix.room_preview", &json!({}), false).is_err());
        assert!(parse("matrix.room_preview", &json!({"room": "matrix.org"}), false).is_err());
        let Ok(MatrixServiceCall::RoomPreview { room, via }) =
            parse("matrix.room_preview", &json!({"room": "#robrix:matrix.org", "via": ["matrix.org", 7]}), false) else { panic!() };
        assert_eq!(room, "#robrix:matrix.org");
        assert_eq!(via, vec!["matrix.org"]);
        let Ok(MatrixServiceCall::RoomPreview { room, via }) =
            parse("matrix.room_preview", &json!({"room": "!abc:x"}), false) else { panic!() };
        assert_eq!(room, "!abc:x");
        assert!(via.is_empty());
    }

    #[test]
    fn rooms_info_needs_a_room_id() {
        assert!(parse("matrix.rooms_info", &json!({}), false).is_err());
        assert!(parse("matrix.rooms_info", &json!({"room_id": "#alias:x"}), false).is_err());
        let Ok(MatrixServiceCall::RoomsInfo { room_id }) =
            parse("matrix.rooms_info", &json!({"room_id": " !abc:x "}), false) else { panic!() };
        assert_eq!(room_id, "!abc:x");
    }

    #[test]
    fn rooms_messages_needs_a_room_id_and_clamps_limit() {
        assert!(parse("matrix.rooms_messages", &json!({"limit": 5}), false).is_err());
        let Ok(MatrixServiceCall::RoomsMessages { room_id, limit }) =
            parse("matrix.rooms_messages", &json!({"room_id": "!abc:x", "limit": 999}), false) else { panic!() };
        assert_eq!((room_id.as_str(), limit), ("!abc:x", 30));
        let Ok(MatrixServiceCall::RoomsMessages { limit, .. }) =
            parse("matrix.rooms_messages", &json!({"room_id": "!abc:x"}), false) else { panic!() };
        assert_eq!(limit, 10);
    }

    #[test]
    fn space_services_parse() {
        assert!(matches!(parse("matrix.spaces", &json!({}), false), Ok(MatrixServiceCall::Spaces)));
        assert!(parse("matrix.space_info", &json!({}), false).is_err());
        assert!(parse("matrix.space_rooms", &json!({"space_id": "bad"}), false).is_err());
        let Ok(MatrixServiceCall::SpaceInfo { space_id }) =
            parse("matrix.space_info", &json!({"space_id": "!s:x"}), false) else { panic!() };
        assert_eq!(space_id, "!s:x");
        let Ok(MatrixServiceCall::SpaceRooms { space_id }) =
            parse("matrix.space_rooms", &json!({"space_id": "!s:x"}), false) else { panic!() };
        assert_eq!(space_id, "!s:x");
    }

    #[test]
    fn user_services_need_a_user_id() {
        assert!(parse("matrix.user_profile", &json!({}), false).is_err());
        assert!(parse("matrix.dm_find", &json!({"user_id": "alice"}), false).is_err());
        let Ok(MatrixServiceCall::UserProfile { user_id }) =
            parse("matrix.user_profile", &json!({"user_id": "@alice:x"}), false) else { panic!() };
        assert_eq!(user_id, "@alice:x");
        let Ok(MatrixServiceCall::DmFind { user_id }) =
            parse("matrix.dm_find", &json!({"user_id": "@alice:x"}), false) else { panic!() };
        assert_eq!(user_id, "@alice:x");
    }

    #[test]
    fn account_services_parse_without_a_room() {
        assert!(matches!(parse("matrix.device", &json!({}), false), Ok(MatrixServiceCall::Device)));
        assert!(matches!(parse("matrix.account_info", &json!({}), false), Ok(MatrixServiceCall::AccountInfo)));
        assert!(matches!(parse("matrix.ignored_users", &json!({}), false), Ok(MatrixServiceCall::IgnoredUsers)));
        assert!(matches!(parse("matrix.account_data", &json!({"key": "prefs"}), false), Ok(MatrixServiceCall::AccountData { .. })));
        assert!(matches!(parse("matrix.set_account_data", &json!({"key": "prefs", "content": null}), false),
            Ok(MatrixServiceCall::SetAccountData { .. })));
    }
}

#[cfg(test)]
mod account_data_tests {
    use super::*;
    use crate::account_data::{CONTENT_NOT_OBJECT, CONTENT_TOO_COMPLEX, CONTENT_TOO_LARGE, INVALID_KEY, MAX_CONTENT_BYTES, MAX_KEYS};
    use serde_json::json;

    fn read(service: &str, args: serde_json::Value) -> Result<(Vec<String>, bool), String> {
        match parse(service, &args, true)? {
            MatrixServiceCall::AccountData { keys, is_batch } if service == "matrix.account_data" => Ok((keys, is_batch)),
            MatrixServiceCall::RoomAccountData { keys, is_batch } if service == "matrix.room_account_data" => Ok((keys, is_batch)),
            _ => panic!("{service} parsed to another call"),
        }
    }

    fn write(service: &str, args: serde_json::Value) -> Result<(String, Option<serde_json::Value>), String> {
        match parse(service, &args, true)? {
            MatrixServiceCall::SetAccountData { key, content } if service == "matrix.set_account_data" => Ok((key, content)),
            MatrixServiceCall::SetRoomAccountData { key, content } if service == "matrix.set_room_account_data" => Ok((key, content)),
            _ => panic!("{service} parsed to another call"),
        }
    }

    #[test]
    fn reads_take_one_key_or_a_deduped_batch() {
        for service in ["matrix.account_data", "matrix.room_account_data"] {
            assert_eq!(read(service, json!({"key": "prefs"})), Ok((vec!["prefs".to_string()], false)));
            assert_eq!(read(service, json!({"keys": ["prefs"]})), Ok((vec!["prefs".to_string()], true)), "a batch of one keeps its shape");
            assert_eq!(read(service, json!({"keys": ["b", "a", "b", "a", "c"]})),
                Ok((["b", "a", "c"].map(String::from).to_vec(), true)), "deduped in asked order");
            let most: Vec<String> = (0..MAX_KEYS).map(|n| format!("k{n}")).collect();
            assert_eq!(read(service, json!({"keys": most})), Ok((most.clone(), true)));
            let too_many: Vec<String> = (0..=MAX_KEYS).map(|n| format!("k{n}")).collect();
            for args in [json!({"keys": []}), json!({"keys": too_many}), json!({"keys": "prefs"}), json!({"keys": null})] {
                assert_eq!(read(service, args).unwrap_err(), format!("{service} {{keys}} must list 1 to {MAX_KEYS} keys"));
            }
            assert_eq!(read(service, json!({"key": "prefs", "keys": ["prefs"]})).unwrap_err(), format!("{service} takes {{key}} or {{keys}}, not both"));
            for args in [json!({}), json!(null), json!([]), json!("prefs")] {
                assert_eq!(read(service, args).unwrap_err(), format!("{service} needs {{key}} or {{keys}}"));
            }
            for args in [json!({"key": "Prefs"}), json!({"key": ""}), json!({"key": 3}), json!({"key": null}), json!({"key": "m.direct"}),
                json!({"key": "rs.robius.a2app.appdata.other.prefs"}), json!({"keys": ["ok", "not ok"]}), json!({"keys": ["ok", 1]})]
            {
                assert_eq!(read(service, args).unwrap_err(), INVALID_KEY);
            }
        }
    }

    #[test]
    fn robrix_chooses_the_type_so_other_arguments_are_refused() {
        for (service, args) in [
            ("matrix.account_data", json!({"key": "prefs", "type": "m.direct"})),
            ("matrix.room_account_data", json!({"key": "prefs", "room_id": "!other:s"})),
            ("matrix.account_data", json!({"keys": ["prefs"], "app_id": "other"})),
            ("matrix.room_account_data", json!({"event_type": "m.fully_read"})),
        ] {
            assert_eq!(read(service, args).unwrap_err(), format!("{service} takes only {{key}} or {{keys}}: Robrix chooses the event type."));
        }
        for (service, args) in [
            ("matrix.set_account_data", json!({"key": "prefs", "content": {}, "type": "m.direct"})),
            ("matrix.set_room_account_data", json!({"key": "prefs", "content": {}, "room_id": "!other:s"})),
            ("matrix.set_account_data", json!({"key": "prefs", "content": {}, "keys": ["prefs"]})),
        ] {
            assert_eq!(write(service, args).unwrap_err(), format!("{service} takes only {{key, content}}: Robrix chooses the event type."));
        }
    }

    #[test]
    fn writes_take_an_object_within_both_caps_or_null_to_delete() {
        for service in ["matrix.set_account_data", "matrix.set_room_account_data"] {
            let content = json!({"theme": "dark", "sizes": [1, 2]});
            assert_eq!(write(service, json!({"key": "prefs", "content": content})), Ok(("prefs".to_string(), Some(content))));
            assert_eq!(write(service, json!({"key": "prefs", "content": null})), Ok(("prefs".to_string(), None)), "null deletes");
            for args in [json!({"key": "prefs"}), json!({"content": {}}), json!({}), json!(null)] {
                assert!(write(service, args).unwrap_err().starts_with(&format!("{service} needs {{key, content}}")));
            }
            for content in [json!([1]), json!("text"), json!(1), json!(false)] {
                assert_eq!(write(service, json!({"key": "prefs", "content": content})).unwrap_err(), CONTENT_NOT_OBJECT);
            }
            let large = json!({"data": "x".repeat(MAX_CONTENT_BYTES)});
            assert_eq!(write(service, json!({"key": "prefs", "content": large})).unwrap_err(), CONTENT_TOO_LARGE);
            let grid = json!({"cells": vec![vec![1; 64]; 64]});
            assert_eq!(write(service, json!({"key": "grid", "content": grid})).unwrap_err(), CONTENT_TOO_COMPLEX,
                "small JSON that a review could not show is refused before any effect review");
            for args in [json!({"key": "Prefs", "content": {}}), json!({"key": 1, "content": {}}), json!({"key": "a.b", "content": null})] {
                assert_eq!(write(service, args).unwrap_err(), INVALID_KEY);
            }
        }
    }

    #[test]
    fn room_notes_need_a_room_but_global_data_does_not() {
        for (service, args) in [("matrix.room_account_data", json!({"key": "notes"})),
            ("matrix.set_room_account_data", json!({"key": "notes", "content": {}}))]
        {
            assert_eq!(parse(service, &args, false).err().as_deref(), Some("this mini-app is not attached to a room"));
            assert!(parse(service, &args, true).is_ok());
        }
        assert!(matches!(parse("matrix.account_data", &json!({"keys": ["a", "b"]}), false),
            Ok(MatrixServiceCall::AccountData { is_batch: true, .. })));
        assert!(matches!(parse("matrix.set_account_data", &json!({"key": "a", "content": {"x": 1}}), false),
            Ok(MatrixServiceCall::SetAccountData { content: Some(_), .. })));
    }

    #[test]
    fn a_read_costs_one_token_however_many_keys_and_a_write_costs_a_send() {
        for service in ["matrix.account_data", "matrix.room_account_data"] {
            assert_eq!(cost(service), 1.0);
        }
        for service in ["matrix.set_account_data", "matrix.set_room_account_data"] {
            assert_eq!(cost(service), 8.0);
        }
    }
}

#[cfg(test)]
mod send_tests {
    use super::*;
    use serde_json::json;

    fn parse_ok(service: &str, args: serde_json::Value) -> MatrixServiceCall {
        parse(service, &args, true).unwrap_or_else(|e| panic!("{service} should parse: {e}"))
    }

    fn parse_err(service: &str, args: serde_json::Value) -> String {
        match parse(service, &args, true) {
            Ok(_) => panic!("{service} should be rejected"),
            Err(e) => e,
        }
    }

    #[test]
    fn media_send_keeps_exact_inline_bytes_and_caption_for_the_attached_room() {
        let args = json!({"data_base64":"AAH/","filename":"fixture.bin","mime_type":"Application/Octet-Stream","caption":"  Please review this file  "});
        let MatrixServiceCall::SendMedia { data_base64, filename, mime_type, caption } = parse_ok("matrix.send_media", args.clone())
            else { panic!("expected a native media send") };
        assert_eq!(data_base64, "AAH/");
        assert_eq!(filename, "fixture.bin");
        assert_eq!(mime_type, "application/octet-stream");
        assert_eq!(caption.as_deref(), Some("  Please review this file  "));
        assert!(matches!(parse("matrix.send_media", &args, false), Err(error) if error.contains("not attached to a room")));
        assert_eq!(cost("matrix.send_media"), 8.0);
    }

    #[test]
    fn media_send_rejects_remote_targets_paths_invalid_or_oversized_inline_data() {
        let valid = json!({"data_base64":"bWVkaWE=","filename":"fixture.txt","mime_type":"text/plain"});
        for (field, value) in [
            ("room_id", json!("!another:test")), ("room_id", serde_json::Value::Null),
            ("path", json!("/private/file")), ("url", json!("https://example.test/file")),
            ("filename", json!("../file")), ("mime_type", json!("image/*")),
            ("caption", json!(true)), ("data_base64", json!("data:image/png;base64,bWVkaWE=")),
            ("data_base64", json!("bWVkaWE")),
            ("data_base64", json!("x".repeat(super::super::MAX_COMPOSER_MEDIA_BASE64_BYTES + 1))),
        ] {
            let mut args = valid.clone();
            args[field] = value;
            assert!(parse("matrix.send_media", &args, true).is_err(), "accepted invalid {field}");
        }
        for field in ["data_base64", "filename", "mime_type"] {
            let mut args = valid.clone();
            args.as_object_mut().unwrap().remove(field);
            assert!(parse_err("matrix.send_media", args).contains(field));
        }
        let mut args = valid;
        args["unknown"] = json!("anything");
        assert!(parse_err("matrix.send_media", args).starts_with("matrix.send_media"));
    }

    #[test]
    fn reply_needs_an_event_and_a_body() {
        let MatrixServiceCall::Reply { event_id, body, in_thread } =
            parse_ok("matrix.reply", json!({"event_id": "$e", "body": "  hi  "})) else { panic!() };
        assert_eq!(event_id, "$e");
        assert_eq!(body, "hi", "the body is trimmed");
        assert!(!in_thread);
        assert_eq!(parse_err("matrix.reply", json!({"body": "hi"})), "matrix.reply needs {event_id}");
        assert_eq!(parse_err("matrix.reply", json!({"event_id": "$e", "body": "   "})), "matrix.reply needs {body}");
        assert!(parse_err("matrix.reply", json!({"event_id": "$e", "body": "x".repeat(4097)})).contains("too long"));
        assert!(matches!(
            parse_ok("matrix.reply", json!({"event_id": "$e", "body": "x".repeat(4096)})),
            MatrixServiceCall::Reply { .. }
        ));
    }

    #[test]
    fn a2ui_action_needs_a_message_and_a_json_object_of_at_most_44_kib() {
        // A JSON object of exactly `len` bytes.
        let object_of = |len: usize| format!(r#"{{"data":"{}"}}"#, "x".repeat(len - 11));
        let action = |content: String| json!({"event_id": "$surface:test", "content": content});
        let MatrixServiceCall::A2uiAction { event_id, content } =
            parse_ok("matrix.a2ui_action", action(object_of(MAX_A2UI_ACTION_BYTES))) else { panic!() };
        assert_eq!(event_id, "$surface:test");
        assert_eq!(content.len(), 44 * 1024, "the bound itself is accepted");
        assert!(matches!(parse_ok("matrix.a2ui_action", action(object_of(16 * 1024 + 1))), MatrixServiceCall::A2uiAction { .. }),
            "a sendDataModel answer bigger than the old 16 KiB cap fits");
        assert_eq!(
            parse_err("matrix.a2ui_action", action(object_of(MAX_A2UI_ACTION_BYTES + 1))),
            "matrix.a2ui_action needs {content}: a JSON object of at most 44 KiB",
        );
        for content in ["", "[1]", "\"text\"", "{\"open\":"] {
            assert!(parse_err("matrix.a2ui_action", action(content.into())).contains("{content}"), "{content}");
        }
        assert_eq!(parse_err("matrix.a2ui_action", json!({"content": "{}"})), "matrix.a2ui_action needs {event_id}");
        assert_eq!(parse_err("matrix.a2ui_action", json!({"event_id": "surface", "content": "{}"})), "matrix.a2ui_action needs {event_id}");
        assert!(matches!(parse("matrix.a2ui_action", &action("{}".into()), false), Err(error) if error.contains("not attached to a room")));
    }

    #[test]
    fn thread_reply_is_a_threaded_reply() {
        let MatrixServiceCall::Reply { in_thread, .. } =
            parse_ok("matrix.thread_reply", json!({"event_id": "$root", "body": "hi"})) else { panic!() };
        assert!(in_thread);
        assert_eq!(parse_err("matrix.thread_reply", json!({"event_id": "$root"})), "matrix.thread_reply needs {body}");
    }

    #[test]
    fn react_bounds_the_key() {
        let MatrixServiceCall::React { event_id, key } =
            parse_ok("matrix.react", json!({"event_id": "$e", "key": "👍"})) else { panic!() };
        assert_eq!((event_id.as_str(), key.as_str()), ("$e", "👍"));
        assert_eq!(parse_err("matrix.react", json!({"event_id": "$e", "key": ""})), "matrix.react needs {key}");
        assert_eq!(parse_err("matrix.react", json!({"key": "👍"})), "matrix.react needs {event_id}");
        // Counted in chars, not bytes, so a multi-byte key isn't short-changed.
        assert!(matches!(
            parse_ok("matrix.react", json!({"event_id": "$e", "key": "é".repeat(32)})),
            MatrixServiceCall::React { .. }
        ));
        assert!(parse_err("matrix.react", json!({"event_id": "$e", "key": "é".repeat(33)})).contains("32 characters"));
    }

    #[test]
    fn typing_needs_a_real_bool() {
        assert!(matches!(parse_ok("matrix.typing", json!({"typing": true})), MatrixServiceCall::Typing { typing: true }));
        assert_eq!(parse_err("matrix.typing", json!({"typing": "yes"})), "matrix.typing needs {typing: bool}");
        assert_eq!(parse_err("matrix.typing", json!({})), "matrix.typing needs {typing: bool}");
    }

    #[test]
    fn read_receipt_event_is_optional() {
        assert!(matches!(parse_ok("matrix.read_receipt", json!({})), MatrixServiceCall::ReadReceipt { event_id: None }));
        assert!(matches!(parse_ok("matrix.read_receipt", json!({"event_id": " "})), MatrixServiceCall::ReadReceipt { event_id: None }));
        let MatrixServiceCall::ReadReceipt { event_id: Some(id) } =
            parse_ok("matrix.read_receipt", json!({"event_id": "$e"})) else { panic!() };
        assert_eq!(id, "$e");
    }

    #[test]
    fn pin_needs_an_event_and_a_direction() {
        assert!(matches!(
            parse_ok("matrix.pin", json!({"event_id": "$e", "pinned": false})),
            MatrixServiceCall::Pin { pinned: false, .. }
        ));
        assert_eq!(parse_err("matrix.pin", json!({"event_id": "$e"})), "matrix.pin needs {pinned: bool}");
        assert_eq!(parse_err("matrix.pin", json!({"pinned": true})), "matrix.pin needs {event_id}");
    }

    #[test]
    fn room_flags_share_one_shape() {
        assert!(matches!(
            parse_ok("matrix.favorite", json!({"on": true})),
            MatrixServiceCall::RoomFlag { flag: RoomFlag::Favorite, on: true }
        ));
        assert!(matches!(
            parse_ok("matrix.low_priority", json!({"on": false})),
            MatrixServiceCall::RoomFlag { flag: RoomFlag::LowPriority, on: false }
        ));
        assert!(matches!(
            parse_ok("matrix.mark_unread", json!({"on": true})),
            MatrixServiceCall::RoomFlag { flag: RoomFlag::Unread, on: true }
        ));
        assert_eq!(parse_err("matrix.mark_unread", json!({"on": 1})), "matrix.mark_unread needs {on: bool}");
        assert_eq!(
            parse("matrix.favorite", &json!({"on": true}), false).err().unwrap(),
            "this mini-app is not attached to a room"
        );
    }

    #[test]
    fn rooms_send_works_without_a_room_but_needs_one_named() {
        assert!(matches!(
            parse("matrix.rooms_send", &json!({"room_id": "!r:s", "body": "hi"}), false),
            Ok(MatrixServiceCall::RoomsSend { .. })
        ));
        assert_eq!(parse_err("matrix.rooms_send", json!({"body": "hi"})), "matrix.rooms_send needs {room_id}");
        assert_eq!(parse_err("matrix.rooms_send", json!({"room_id": "!r:s"})), "matrix.rooms_send needs {body}");
        assert!(parse_err("matrix.rooms_send", json!({"room_id": "!r:s", "body": "x".repeat(4097)})).contains("too long"));
    }

    #[test]
    fn invite_is_room_scoped() {
        assert!(matches!(parse_ok("matrix.invite", json!({"user_id": "@u:s"})), MatrixServiceCall::Invite { .. }));
        assert_eq!(parse_err("matrix.invite", json!({})), "matrix.invite needs {user_id}");
        assert_eq!(
            parse("matrix.invite", &json!({"user_id": "@u:s"}), false).err().unwrap(),
            "this mini-app is not attached to a room"
        );
    }

    #[test]
    fn join_takes_an_optional_via_list() {
        let MatrixServiceCall::Join { room, via } =
            parse("matrix.join", &json!({"room": "#a:s", "via": ["s", 7, "t"]}), false).unwrap() else { panic!() };
        assert_eq!(room, "#a:s");
        assert_eq!(via, ["s", "t"], "non-string servers are dropped");
        let MatrixServiceCall::Join { via, .. } =
            parse("matrix.join", &json!({"room": "!r:s"}), false).unwrap() else { panic!() };
        assert!(via.is_empty());
        assert_eq!(parse("matrix.join", &json!({}), false).err().unwrap(), "matrix.join needs {room}");
    }

    #[test]
    fn invite_respond_needs_a_room_and_an_answer() {
        assert!(matches!(
            parse("matrix.invite_respond", &json!({"room_id": "!r:s", "accept": false}), false),
            Ok(MatrixServiceCall::InviteRespond { accept: false, .. })
        ));
        assert_eq!(
            parse("matrix.invite_respond", &json!({"room_id": "!r:s"}), false).err().unwrap(),
            "matrix.invite_respond needs {accept: bool}"
        );
        assert_eq!(
            parse("matrix.invite_respond", &json!({"accept": true}), false).err().unwrap(),
            "matrix.invite_respond needs {room_id}"
        );
    }

    #[test]
    fn dm_open_needs_a_user() {
        assert!(matches!(
            parse("matrix.dm_open", &json!({"user_id": "@u:s"}), false),
            Ok(MatrixServiceCall::DmOpen { .. })
        ));
        assert_eq!(parse("matrix.dm_open", &json!({"user_id": ""}), false).err().unwrap(), "matrix.dm_open needs {user_id}");
    }
}
