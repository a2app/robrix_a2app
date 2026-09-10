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

    /// Posts `text` into ANOTHER joined room as an `m.notice` message, gated
    /// per room (the user is asked the first time this agent posts into each
    /// room).
    fn post_room_message(&self, room: &str, text: &str) -> Result<String, String>;

    /// One capability-gated attached-room read. The host hands it to the UI
    /// thread, where the runtime decides whether this session's room subject
    /// may exercise the mapped capability (prompting the user on first use)
    /// before fetching the data and returning it as JSON text.
    fn read_tool(&self, kind: ReadToolKind) -> Result<String, String>;
}

/// The read the model asked for, parsed and clamped by its [`Tool`] impl into
/// the shape the executor (and the async matrix worker) needs. One variant per
/// tool; every variant except [`ReadToolKind::Memory`] maps 1:1 onto a catalog
/// capability, so the runtime can gate the call before any data moves.
/// `Memory` is different by design: reading the room's `ai_reply` history is
/// the agent recalling its OWN past turns — room plumbing like `send_message`
/// — so it is never gated or prompted. It still rides the read path because
/// fetching state events needs the async matrix worker.
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
    /// messages of another JOINED room the model names. Gated like any other
    /// read: the user is asked the first time the AI reads a room outside
    /// this one.
    OtherRoom { room: String, limit: u32 },
    /// `list_rooms` → `matrix.rooms.list`: the joined rooms and DMs the
    /// model may offer to read, with names and ids.
    ListRooms,
    /// `read_room_memory` (ungated): this room's recent `ai_reply` turns —
    /// the agent's OWN past replies and tool calls. Not a catalog capability:
    /// the runtime treats it as always allowed, like `send_message`.
    Memory { limit: u32 },
}

impl ReadToolKind {
    /// The catalog capability this tool exercises — what the user is asked
    /// about and what the access record names. [`Self::Memory`] has none (it
    /// is ungated room plumbing); the runtime special-cases it before ever
    /// consulting this.
    pub fn capability_id(&self) -> &'static str {
        match self {
            ReadToolKind::Messages { .. } => "matrix.room.messages.read",
            ReadToolKind::Older { .. } => "matrix.room.messages.paginate",
            ReadToolKind::Info => "matrix.room.info.read",
            ReadToolKind::OtherRoom { .. } => "matrix.rooms.messages.read",
            ReadToolKind::ListRooms => "matrix.rooms.list",
            ReadToolKind::Memory { .. } => "",
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
/// it is deliberately not listed anywhere here — and neither is
/// `read_room_memory`, which is the same kind of ungated plumbing (the agent
/// recalling its own past turns).
pub const AI_ROOM_READ_CAP_IDS: &[&str] = &[
    "matrix.room.messages.read",
    "matrix.room.messages.paginate",
    "matrix.room.info.read",
    "matrix.rooms.messages.read",
    "matrix.rooms.list",
];

/// Everything an AI session may do that sits in the mini-app capability
/// catalog: the read tools above, the generator (`launch_splash_app`), and
/// posting into the user's other rooms (`post_room_message` — gated per
/// room, see the runtime). What the runtime's gate checks against, so every
/// offered capability is declared — a profile list can only grow by editing
/// this array.
pub const AI_ROOM_SESSION_CAP_IDS: &[&str] = &[
    "matrix.room.messages.read",
    "matrix.room.messages.paginate",
    "matrix.room.info.read",
    "matrix.rooms.messages.read",
    "matrix.rooms.list",
    "apps.generate",
    "matrix.rooms.message.send",
];

/// The MCP tool name for a read kind — what shows on the room's `ai_reply`
/// receipt chip.
pub fn read_tool_name(kind: &ReadToolKind) -> &'static str {
    match kind {
        ReadToolKind::Messages { .. } => "read_room_messages",
        ReadToolKind::Older { .. } => "read_older_messages",
        ReadToolKind::Info => "room_info",
        ReadToolKind::OtherRoom { .. } => "read_other_room_messages",
        ReadToolKind::ListRooms => "list_rooms",
        ReadToolKind::Memory { .. } => "read_room_memory",
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
         rows. Each row has the sender, the sender_id, the event_id, the \
         room_id, the body, and whether the user has already read it \
         (unread: true means it arrived after the user's last read receipt). \
         Use it to catch up on conversation you have not been told about — \
         when asked to summarize, focus on the rows where unread is true."
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
         you have — to get the messages that came before it. Rows carry the \
         sender, sender_id, event_id, room_id, body and an unread flag, as \
         in read_room_messages."
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

/// `read_other_room_messages` — read a JOINED room the model names. Gated by
/// `matrix.rooms.messages.read`; the user is asked the first time the AI
/// reads a room outside this one, and a granted answer lets it read any
/// joined room it names.
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
        "Returns the recent text messages of another joined room you name, as \
         JSON rows with the sender, sender_id, event_id, the row's own \
         room_id, body, and an unread flag (true when the user hasn't read it \
         yet). The user is asked to allow the first read of a room outside \
         this one; once allowed you may read any of their rooms. Use \
         list_rooms to see which rooms exist."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "room": {
                    "type": "string",
                    "description": "The matrix room id to read, e.g. !abc:server.org (see list_rooms).",
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

/// `list_rooms` — the joined rooms and DMs the model may offer to read.
/// Gated by `matrix.rooms.list`; the user is asked on first use.
pub struct ListRoomsTool {
    host: Arc<dyn AiHost>,
}

impl ListRoomsTool {
    pub fn new(host: Arc<dyn AiHost>) -> Self {
        Self { host }
    }
}

impl Tool for ListRoomsTool {
    fn name(&self) -> &str {
        "list_rooms"
    }

    fn description(&self) -> &str {
        "Lists the user's joined rooms and direct chats: name, room id, \
         whether it is a direct chat or a space, member count, encryption, \
         and unread/mentions counts. Call it to find the room id to pass to \
         read_other_room_messages when the user asks about a room."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "additionalProperties": false })
    }

    fn call(&self, _arguments: &Map<String, Value>) -> Result<String, String> {
        self.host.read_tool(ReadToolKind::ListRooms)
    }
}

/// `read_room_memory` — recall the agent's own past replies in this room.
///
/// Ungated room plumbing (like `send_message`): the room's humans see the
/// agent's turns as "AI" cards carrying this text, so this is how the agent
/// recalls what it previously said or built — especially after a restart or
/// when asked to continue earlier work. `read_room_messages` covers what the
/// humans said; this covers what *it* said. Not a catalog capability, so no
/// permission prompt ever gates it.
pub struct ReadRoomMemoryTool {
    host: Arc<dyn AiHost>,
}

impl ReadRoomMemoryTool {
    pub fn new(host: Arc<dyn AiHost>) -> Self {
        Self { host }
    }
}

impl Tool for ReadRoomMemoryTool {
    fn name(&self) -> &str {
        "read_room_memory"
    }

    fn description(&self) -> &str {
        "Returns your own recent replies and actions in this room, oldest \
         first, as JSON rows. The room's humans see your turns as AI cards \
         that carry this text, so this is how you recall what you previously \
         said or built — after a restart, or when the user asks you to \
         continue or build on earlier work. Rows carry the reply text, its \
         timestamp, and any tools you used (building an app, reading another \
         room, sending a message). Use read_room_messages for what the \
         humans said."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "How many of your past turns to return (default 15, max 50).",
                },
            },
            "additionalProperties": false,
        })
    }

    fn call(&self, arguments: &Map<String, Value>) -> Result<String, String> {
        self.host.read_tool(ReadToolKind::Memory { limit: parse_limit(arguments) })
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
        "Posts a message to this room's timeline — the way you reply to the \
         user in words, so use it for every answer, report, and progress \
         update. ALWAYS format your messages — never send a plain, \
         unformatted wall of text. The text supports Markdown: **bold** for \
         key terms, *italic* where it helps, and bullet or numbered lists to \
         structure several points or steps. Mention a person by linking their \
         full matrix id — [Their name](https://matrix.to/#/@user:server) — \
         which the client renders as a clickable avatar pill. Point at a \
         specific message with its permalink — \
         [that message](https://matrix.to/#/!room:server/$event) — which the \
         client renders as a clickable link to that message in that room \
         (event ids come from the read tools). Make a habit of it: every \
         reply should use the formatting and linking that fits, without the \
         user having to ask. For building an app, use launch_splash_app \
         instead."
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

/// `post_room_message` — post an agent-authored message into another joined
/// room as an `m.notice` message.
///
/// A notice is an ordinary `m.room.message` (never the agent's own `ai_reply`
/// state card), so it needs no state-power privilege in the target room.
/// Permission is granted PER ROOM: the user is asked the first time the agent
/// posts into each room it names, and an allowance covers exactly that room.
pub struct PostRoomMessageTool {
    host: Arc<dyn AiHost>,
}

impl PostRoomMessageTool {
    pub fn new(host: Arc<dyn AiHost>) -> Self {
        Self { host }
    }
}

impl Tool for PostRoomMessageTool {
    fn name(&self) -> &str {
        "post_room_message"
    }

    fn description(&self) -> &str {
        "Posts a message to another of the user's joined rooms as a \
         notice, using the same formatting and linking as send_message \
         (Markdown: **bold**, *italic*, lists; link a person with their \
         full matrix id [Name](https://matrix.to/#/@user:server); link a \
         specific message with its permalink \
         [that message](https://matrix.to/#/!room:server/$event)). ALWAYS \
         format your posts the same way you format replies here. Permission \
         is per room — the first time you post into a room the user is \
         asked, and the choice covers exactly that room. Use list_rooms to \
         find the room id, read_other_room_messages to see what is being \
         said there first, and post_room_message to answer or contribute. \
         Do NOT use this for this room's chat — send_message is for here."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "room": {
                    "type": "string",
                    "description": "The matrix room id to post into, e.g. !abc:server.org (see list_rooms).",
                },
                "text": {
                    "type": "string",
                    "description": "The message text to post.",
                },
            },
            "required": ["room", "text"],
            "additionalProperties": false,
        })
    }

    fn call(&self, arguments: &Map<String, Value>) -> Result<String, String> {
        let room = arguments
            .get("room")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .ok_or_else(|| "`post_room_message` needs a `room` id".to_string())?
            .to_string();
        let text = arguments
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| "`post_room_message` needs a string `text`".to_string())?
            .trim();
        if text.is_empty() {
            return Err("`text` must not be empty".to_string());
        }
        self.host.post_room_message(&room, text)
    }
}

/// Builds a session's full tool set and registers it on `server`.
///
/// This is the "add a tool" seam in one place: a new tool is a `Tool` impl
/// plus one line here, and every agent (octos and Claude Code alike, via the
/// bridge) sees it on the next `tools/list`.
pub fn register_session_tools(server: &mut a2app_agent::mcp::McpServer, host: Arc<dyn AiHost>) {
    // Capability-gated attached-room reads first, then the native tools.
    server.add_tool(ReadRoomMessagesTool::new(host.clone()));
    server.add_tool(ReadOlderMessagesTool::new(host.clone()));
    server.add_tool(RoomInfoTool::new(host.clone()));
    server.add_tool(ListRoomsTool::new(host.clone()));
    server.add_tool(ReadOtherRoomMessagesTool::new(host.clone()));
    server.add_tool(LaunchSplashAppTool::new(host.clone()));
    // Ungated native tools (the agent's own room plumbing).
    server.add_tool(ReadRoomMemoryTool::new(host.clone()));
    server.add_tool(SendMessageTool::new(host.clone()));
    server.add_tool(PostRoomMessageTool::new(host));
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
            (ReadToolKind::OtherRoom { room: "!r:s".to_string(), limit: 10 }, "matrix.rooms.messages.read"),
            (ReadToolKind::ListRooms, "matrix.rooms.list"),
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
            ListRooms,
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

    /// `read_room_memory` is the one deliberate exception to "every read kind
    /// maps 1:1 onto a catalog capability": the agent recalling its own past
    /// turns is ungated room plumbing, so it must not surface as a capability
    /// (no permission prompt row, no access record) — yet it still needs a
    /// stable MCP name for logs and a bounded limit.
    #[test]
    fn room_memory_is_ungated_room_plumbing() {
        let kind = ReadToolKind::Memory { limit: 10 };
        assert!(kind.capability().is_none(), "memory is not a catalog capability");
        assert_eq!(read_tool_name(&kind), "read_room_memory");
        assert_eq!(kind.capability_id(), "", "no capability id to gate on");
        let mut args = Map::new();
        args.insert("limit".into(), json!(999));
        assert_eq!(parse_limit(&args), MAX_READ_LIMIT, "memory reads stay bounded too");
    }
}
