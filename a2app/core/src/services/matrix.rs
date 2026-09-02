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

    // --- spaces ---

    // --- account ---

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
    matches!(service, "matrix.profile" | "matrix.rooms_list" | "matrix.search_rooms")
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

        // --- spaces ---

        // --- account ---

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
        // ----- account -----
        "matrix.profile" => 1.0,
        // ----- send: speaks as the user -----
        "matrix.send_message" => 5.0,

        // --- spaces ---

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
