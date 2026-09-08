//! The host tools Robrix exposes to one agent session.
//!
//! Each session builds these with an [`AiHost`] — the Robrix-side object that
//! actually does the work (run the app-generation pipeline, post to the
//! session's room, read the room under its capability grant). The [`Tool`]
//! impls below are deliberately thin: they exist to give the model the right
//! *shape* to call (name, description, argument schema) and to hand the parsed
//! arguments to the host. All the state lives in the host, which is what keeps
//! this list trivially extensible.
//!
//! Some tools are *capability-gated*: they map onto a row of the mini-app
//! capability catalog (see [`ReadToolKind`]) and the session's runtime
//! decides, against the room's permission subject, whether the call may run —
//! prompting the user on first use, exactly as a mini-app would. Those tools
//! carry their capability id with them so the executor never has to guess.

use std::sync::Arc;

use a2app_agent::mcp::Tool;
use serde_json::{Map, Value, json};

use a2app_core::capabilities::by_id;

/// What Robrix can do for a session's tools.
///
/// Implemented by the session host on the UI thread — the only place with the
/// state a tool needs (the app registry to install into, the Matrix room to
/// post to, the running `Generation`). Each method returns the text payload the
/// model sees: `Ok` text for a success, `Err` text for a failure the model
/// should be told about. `launch_splash_app`'s success payload is a small JSON
/// summary the model can quote; everything else is prose.
pub trait AiHost: Send + Sync {
    /// Generates and installs a room-scoped mini-app from a natural-language
    /// description, then runs it. Returns a structured summary on success:
    /// `{"app_id":…,"name":…,"status":"installed_and_running"}`.
    fn launch_splash_app(&self, description: &str) -> Result<String, String>;

    /// Posts `text` into the room associated with this session.
    fn send_room_message(&self, text: &str) -> Result<String, String>;

    /// One capability-gated attached-room read. The host hands it to the UI
    /// thread, where the runtime decides whether this session's room subject
    /// may exercise the mapped capability (prompting the user on first use)
    /// before fetching the data and returning it as JSON text.
    fn read_tool(&self, kind: ReadToolKind) -> Result<String, String>;
}

/// The read the model asked for, parsed and clamped by its [`Tool`] impl into
/// the shape the executor (and the async matrix worker) needs. One variant per
/// tool; each maps 1:1 onto a catalog capability, so the runtime can gate the
/// call before any data moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadToolKind {
    /// `read_room_messages` → `matrix.room.messages.read`.
    Messages { limit: u32 },
    /// `read_older_messages` → `matrix.room.messages.paginate`. `before` is
    /// the event id of the oldest message already seen (one page further
    /// back than the recent window).
    Older { before: Option<String>, limit: u32 },
    /// `room_info` → `matrix.room.info.read`.
    Info,
    /// `read_other_room_messages` → `matrix.rooms.messages.read`: recent
    /// messages of another JOINED room the user has allowlisted for this
    /// AI in the room's panel (`/ai allow <room>`). `room` is the target
    /// room's id, as a string for the tool's schema.
    OtherRoom { room: String, limit: u32 },
}

impl ReadToolKind {
    /// The catalog capability this tool exercises — what the user is asked
    /// about and what the access record names.
    pub fn capability_id(&self) -> &'static str {
        match self {
            ReadToolKind::Messages { .. } => "matrix.room.messages.read",
            ReadToolKind::Older { .. } => "matrix.room.messages.paginate",
            ReadToolKind::Info => "matrix.room.info.read",
            ReadToolKind::OtherRoom { .. } => "matrix.rooms.messages.read",
        }
    }

    /// The catalog row, if this build still carries it.
    pub fn capability(&self) -> Option<&'static a2app_core::capabilities::Capability> {
        by_id(self.capability_id())
    }
}

/// The capability ids an AI session offers as gated attached-room reads —
/// the read half of its declaration profile (see
/// [`AI_ROOM_SESSION_CAP_IDS`], which adds the generator). Kept beside the
/// tool impls so the tool list and the runtime's gate cannot disagree. The
/// native `send_message` tool is room plumbing, not a catalog capability, so
/// it is deliberately not listed anywhere here.
pub const AI_ROOM_READ_CAP_IDS: &[&str] = &[
    "matrix.room.messages.read",
    "matrix.room.messages.paginate",
    "matrix.room.info.read",
];

/// Everything an AI session may do that sits in the mini-app capability
/// catalog: the read tools above plus the generator (`launch_splash_app`).
/// What the runtime's gate checks against, so every offered capability is
/// declared — a profile list can only grow by editing this array.
pub const AI_ROOM_SESSION_CAP_IDS: &[&str] = &[
    "matrix.room.messages.read",
    "matrix.room.messages.paginate",
    "matrix.room.info.read",
    "matrix.rooms.messages.read",
    "apps.generate",
];

/// The MCP tool name for a read kind — what shows on the room's `ai_reply`
/// receipt chip.
pub fn read_tool_name(kind: &ReadToolKind) -> &'static str {
    match kind {
        ReadToolKind::Messages { .. } => "read_room_messages",
        ReadToolKind::Older { .. } => "read_older_messages",
        ReadToolKind::Info => "room_info",
        ReadToolKind::OtherRoom { .. } => "read_other_room_messages",
    }
}

/// How many messages a read tool returns unless the model says otherwise.
const DEFAULT_READ_LIMIT: u32 = 15;
/// The most a read tool will ever return in one call, however the model asks.
const MAX_READ_LIMIT: u32 = 50;

fn parse_limit(arguments: &Map<String, Value>) -> u32 {
    arguments
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n.clamp(1, MAX_READ_LIMIT as u64) as u32)
        .unwrap_or(DEFAULT_READ_LIMIT)
}

/// `read_room_messages` — catch up on the room's recent conversation.
pub struct ReadRoomMessagesTool {
    host: Arc<dyn AiHost>,
}

impl ReadRoomMessagesTool {
    pub fn new(host: Arc<dyn AiHost>) -> Self {
        Self { host }
    }
}

impl Tool for ReadRoomMessagesTool {
    fn name(&self) -> &str {
        "read_room_messages"
    }

    fn description(&self) -> &str {
        "Returns this room's recent text messages, oldest first, as JSON \
         rows with the sender and event id. Use it to catch up on \
         conversation you have not been told about."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "How many messages to return (default 15, max 50).",
                },
            },
            "additionalProperties": false,
        })
    }

    fn call(&self, arguments: &Map<String, Value>) -> Result<String, String> {
        self.host.read_tool(ReadToolKind::Messages { limit: parse_limit(arguments) })
    }
}

/// `read_older_messages` — page further back into the room's history.
pub struct ReadOlderMessagesTool {
    host: Arc<dyn AiHost>,
}

impl ReadOlderMessagesTool {
    pub fn new(host: Arc<dyn AiHost>) -> Self {
        Self { host }
    }
}

impl Tool for ReadOlderMessagesTool {
    fn name(&self) -> &str {
        "read_older_messages"
    }

    fn description(&self) -> &str {
        "Returns the page of this room's history older than the messages you \
         already saw. Pass `before` — the event id of the earliest message \
         you have — to get the messages that came before it."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "before": {
                    "type": "string",
                    "description": "Event id to page before; omit for the page just before the recent window.",
                },
                "limit": {
                    "type": "integer",
                    "description": "How many messages to return (default 15, max 50).",
                },
            },
            "additionalProperties": false,
        })
    }

    fn call(&self, arguments: &Map<String, Value>) -> Result<String, String> {
        let before = arguments.get("before").and_then(Value::as_str).map(str::to_string);
        let kind = ReadToolKind::Older { before, limit: parse_limit(arguments) };
        self.host.read_tool(kind)
    }
}

/// `room_info` — the room's cheap public facts.
pub struct RoomInfoTool {
    host: Arc<dyn AiHost>,
}

impl RoomInfoTool {
    pub fn new(host: Arc<dyn AiHost>) -> Self {
        Self { host }
    }
}

impl Tool for RoomInfoTool {
    fn name(&self) -> &str {
        "room_info"
    }

    fn description(&self) -> &str {
        "Returns this room's details: name, topic, member count, encryption, \
         join rule and alias. Call this when you need to know what room you \
         are in or how many people are here."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "additionalProperties": false })
    }

    fn call(&self, _arguments: &Map<String, Value>) -> Result<String, String> {
        self.host.read_tool(ReadToolKind::Info)
    }
}

/// `read_other_room_messages` — read a JOINED room the user allowlisted for
/// this AI. Gated by `matrix.rooms.messages.read` (MatrixRoomsRead) plus the
/// per-room allowlist, so a prompt for the group is not enough on its own:
/// the target room has to have been added with `/ai allow <room>`.
pub struct ReadOtherRoomMessagesTool {
    host: Arc<dyn AiHost>,
}

impl ReadOtherRoomMessagesTool {
    pub fn new(host: Arc<dyn AiHost>) -> Self {
        Self { host }
    }
}

impl Tool for ReadOtherRoomMessagesTool {
    fn name(&self) -> &str {
        "read_other_room_messages"
    }

    fn description(&self) -> &str {
        "Returns the recent text messages of another room the user has let \
         you read, as JSON rows with the sender and event id. The room must \
         have been added with `/ai allow <room>` in its settings."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "room": {
                    "type": "string",
                    "description": "The matrix room id to read, e.g. !abc:server.org.",
                },
                "limit": {
                    "type": "integer",
                    "description": "How many messages to return (default 15, max 50).",
                },
            },
            "required": ["room"],
            "additionalProperties": false,
        })
    }

    fn call(&self, arguments: &Map<String, Value>) -> Result<String, String> {
        let room = arguments
            .get("room")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .ok_or_else(|| "`read_other_room_messages` needs a `room` id".to_string())?
            .to_string();
        self.host.read_tool(ReadToolKind::OtherRoom { room, limit: parse_limit(arguments) })
    }
}

/// `launch_splash_app` — generate a sandboxed mini-app and run it in the
/// session's room.
///
/// The description is everything: the pipeline's own prompt (dialect guide,
/// validation, up to two repair turns) runs against it untouched, exactly as
/// the Mini Apps screen's direct generation does today — this tool is that
/// pipeline, called by the model instead of by a button.
pub struct LaunchSplashAppTool {
    host: Arc<dyn AiHost>,
}

impl LaunchSplashAppTool {
    pub fn new(host: Arc<dyn AiHost>) -> Self {
        Self { host }
    }
}

impl Tool for LaunchSplashAppTool {
    fn name(&self) -> &str {
        "launch_splash_app"
    }

    fn description(&self) -> &str {
        "Builds a sandboxed mini-app from a natural-language description and \
         installs it into this room, then runs it. Call this when the user asks \
         to create, build, make, or modify an app. The result is a JSON summary \
         of the installed app."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "What the app should do, in the user's own words.",
                },
            },
            "required": ["description"],
            "additionalProperties": false,
        })
    }

    fn call(&self, arguments: &Map<String, Value>) -> Result<String, String> {
        let description = arguments
            .get("description")
            .and_then(Value::as_str)
            .ok_or_else(|| "`launch_splash_app` needs a string `description`".to_string())?
            .trim();
        if description.is_empty() {
            return Err("`description` must not be empty".to_string());
        }
        self.host.launch_splash_app(description)
    }
}

/// `send_message` — post an agent-authored message to the session's room.
///
/// This is how the agent talks to the user. It exists so a session can reply
/// in prose at all; without it the model's only output channels are tool calls
/// and the fenced-code reply contract of the app generator.
pub struct SendMessageTool {
    host: Arc<dyn AiHost>,
}

impl SendMessageTool {
    pub fn new(host: Arc<dyn AiHost>) -> Self {
        Self { host }
    }
}

impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        "send_message"
    }

    fn description(&self) -> &str {
        "Posts a plain-text message to this room's timeline. Use it to answer \
         the user's questions and report progress or results in words. For \
         building an app, use launch_splash_app instead."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "The message text to post.",
                },
            },
            "required": ["text"],
            "additionalProperties": false,
        })
    }

    fn call(&self, arguments: &Map<String, Value>) -> Result<String, String> {
        let text = arguments
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| "`send_message` needs a string `text`".to_string())?;
        self.host.send_room_message(text)
    }
}

/// Builds a session's full tool set and registers it on `server`.
///
/// This is the "add a tool" seam in one place: a new tool is a `Tool` impl
/// plus one line here, and every agent (octos and Claude Code alike, via the
/// bridge) sees it on the next `tools/list`.
pub fn register_session_tools(server: &mut a2app_agent::mcp::McpServer, host: Arc<dyn AiHost>) {
    // Capability-gated attached-room reads first, then the two native tools.
    server.add_tool(ReadRoomMessagesTool::new(host.clone()));
    server.add_tool(ReadOlderMessagesTool::new(host.clone()));
    server.add_tool(RoomInfoTool::new(host.clone()));
    server.add_tool(ReadOtherRoomMessagesTool::new(host.clone()));
    server.add_tool(LaunchSplashAppTool::new(host.clone()));
    server.add_tool(SendMessageTool::new(host));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each read tool maps onto exactly the catalog capability its prompt and
    /// access record should name; a typo in `capability_id` is caught here.
    #[test]
    fn read_tool_kinds_resolve_to_catalog_capabilities() {
        let cases = [
            (ReadToolKind::Messages { limit: 10 }, "matrix.room.messages.read"),
            (ReadToolKind::Older { before: None, limit: 10 }, "matrix.room.messages.paginate"),
            (ReadToolKind::Info, "matrix.room.info.read"),
        ];
        for (kind, id) in cases {
            assert_eq!(kind.capability_id(), id);
            assert_eq!(kind.capability().map(|c| c.id), Some(id), "{id} dropped from the catalog");
            assert!(kind.capability().is_some_and(|c| c.is_available()));
        }
    }

    /// The read kinds map 1:1 onto the session's declared capability profile,
    /// so nothing can be offered to a model without being gateable.
    #[test]
    fn read_tool_kinds_are_all_declared_in_the_profile() {
        use ReadToolKind::*;
        for kind in [
            Messages { limit: 1 },
            Older { before: None, limit: 1 },
            Info,
            OtherRoom { room: "!r:s".to_string(), limit: 1 },
        ] {
            let id = kind.capability_id();
            assert!(
                AI_ROOM_SESSION_CAP_IDS.contains(&id),
                "{id} is offered as a tool but not in AI_ROOM_SESSION_CAP_IDS"
            );
            assert!(read_tool_name(&kind).len() > 3);
        }
        for id in AI_ROOM_SESSION_CAP_IDS {
            assert!(by_id(id).is_some_and(|c| c.is_available()), "{id} not available");
        }
    }

    #[test]
    fn read_limits_are_bounded() {
        let mut args = Map::new();
        assert_eq!(parse_limit(&args), DEFAULT_READ_LIMIT);
        args.insert("limit".into(), json!(1000));
        assert_eq!(parse_limit(&args), MAX_READ_LIMIT);
        args.insert("limit".into(), json!(0));
        assert_eq!(parse_limit(&args), 1);
    }
}
