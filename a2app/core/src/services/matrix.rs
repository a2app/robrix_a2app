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
    /// Send a plain text message to the attached room as the user.
    SendMessage { body: String },
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

    // --- send ---

    // --- membership ---
}

/// Which rooms a `matrix.search_*` call covers.
pub enum SearchScope {
    Attached,
    AllJoined,
    /// Room ids as given; the host validates them.
    Rooms(Vec<String>),
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
        "matrix.room_members" => MatrixServiceCall::Members {
            limit: args["limit"].as_u64().unwrap_or(50).clamp(1, 200) as u32,
        },
        "matrix.pinned_events" => MatrixServiceCall::PinnedEvents,
        "matrix.room_threads" => MatrixServiceCall::Threads {
            limit: args["limit"].as_u64().unwrap_or(20).clamp(1, 50) as u32,
        },
        "matrix.send_message" => {
            let body = args["body"].as_str().map(str::trim).unwrap_or_default();
            if body.is_empty() {
                return Err("matrix.send_message needs {body}".into());
            }
            if body.chars().count() > 4096 {
                return Err("message is too long (4096 characters max)".into());
            }
            MatrixServiceCall::SendMessage { body: body.to_string() }
        }

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

        // --- send ---

        // --- membership ---

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
        | "matrix.older_messages" | "matrix.event" | "matrix.read_receipts" => 5.0,
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
        // ----- send: speaks as the user -----
        "matrix.send_message" => 5.0,

        // --- spaces ---
        "matrix.spaces" => 5.0,
        "matrix.space_info" => 1.0,
        "matrix.space_rooms" => 8.0,

        // --- membership ---

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
    }
}
