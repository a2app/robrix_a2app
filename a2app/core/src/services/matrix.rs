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
        "matrix.room_info" => 1.0,
        // ----- room: a worker round-trip -----
        "matrix.read_messages" | "matrix.room_members" | "matrix.pinned_events"
        | "matrix.room_threads" | "matrix.search_room" => 5.0,
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
