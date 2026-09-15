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
/// should be told about. The app tools (`launch_splash_app`, `list_apps`,
/// `launch_app`) return small JSON summaries the model can quote; everything
/// else is prose.
pub trait AiHost: Send + Sync {
    /// Generates and installs a NEW room-scoped mini-app from a natural-language
    /// description, then runs it. This is create-only: it never rewrites an
    /// existing app (use [`AiHost::launch_app`] to run one of those). Returns a
    /// structured summary on success:
    /// `{"app_id":…,"name":…,"status":"installed_and_running"}`.
    fn launch_splash_app(&self, description: &str) -> Result<String, String>;

    /// Lists the installed mini-apps available to this session — the
    /// account-scoped apps plus any app scoped to this session's own room — as
    /// JSON the model reads and picks a [`AiHost::launch_app`] id from. Returns
    /// `{"apps":[{…}]}`.
    fn list_apps(&self) -> Result<String, String>;

    /// Runs an already-installed mini-app in this session's room, by id. This
    /// never generates or rewrites: an unknown (or otherwise unavailable) id is
    /// an error the model can read. Returns
    /// `{"app_id":…,"name":…,"status":"running"}`.
    fn launch_app(&self, app_id: &str) -> Result<String, String>;

    /// Lists the tools the mini-apps in this session's room have registered,
    /// as JSON the model reads before calling
    /// [`AiHost::call_mini_app_tool`]. Each entry carries the `tool` id to
    /// pass back, the app's own name for it, the description and the argument
    /// list. Returns `{"tools":[{…}]}` — empty when no app offers one.
    fn list_mini_app_tools(&self) -> Result<String, String>;

    /// Calls one mini-app tool by the `tool` id from
    /// [`AiHost::list_mini_app_tools`], forwarding `arguments` verbatim. This
    /// is the stable entry point that works for tools registered after the
    /// agent's session started; the app's own answer is the result.
    fn call_mini_app_tool(
        &self,
        tool: &str,
        arguments: Map<String, Value>,
    ) -> Result<String, String>;

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

/// The UI-thread rendezvous an app-registered tool uses: `invoke` marshals the
/// call to the session's runtime (which delivers it to the owning isolate and
/// blocks until the app answers) and returns the app's text result. Split from
/// [`AiHost`] so the built-in tools don't have to grow a mini-app method, and
/// so the MCP protocol layer never has to know apps exist.
pub trait MiniAppToolBridge: Send + Sync {
    fn invoke(&self, tool: &str, arguments: &Map<String, Value>) -> Result<String, String>;
}

/// A tool a running mini-app registered with this session's MCP server.
///
/// The name is namespaced by the runtime (`app_<id>_<name>`) so an app can
/// never shadow a built-in or another app's tool; the description and schema
/// are the app's own, shown to the user before the tool goes live. `call`
/// blocks the serve thread while the app works — exactly like the built-in
/// tools that marshal to the UI thread — and the runtime bounds that wait.
pub struct MiniAppTool {
    full_name: String,
    description: String,
    schema: Value,
    bridge: Arc<dyn MiniAppToolBridge>,
}

impl MiniAppTool {
    pub fn new(
        full_name: impl Into<String>,
        description: impl Into<String>,
        schema: Value,
        bridge: Arc<dyn MiniAppToolBridge>,
    ) -> Self {
        Self {
            full_name: full_name.into(),
            description: description.into(),
            schema,
            bridge,
        }
    }
}

impl Tool for MiniAppTool {
    fn name(&self) -> &str {
        &self.full_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.schema.clone()
    }

    fn call(&self, arguments: &Map<String, Value>) -> Result<String, String> {
        self.bridge.invoke(&self.full_name, arguments)
    }
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
    /// `list_spaces` → `matrix.spaces.list`: the spaces the user has joined,
    /// with names and ids. Spaces are how the user's rooms are organised, so
    /// this is the map the model uses before drilling into one.
    ListSpaces,
    /// `space_info` → `matrix.space.info.read`: one named space's details
    /// (name, topic, member/room counts, join rule, world-readable).
    SpaceInfo { space: String },
    /// `list_space_rooms` → `matrix.space.rooms.list`: the child rooms and
    /// subspaces of one named space. The separate tool keeps the (potentially
    /// large, per-space) hierarchy walk behind its own grant rather than
    /// folding every room of every space into `list_spaces`.
    SpaceRooms { space: String },
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
            ReadToolKind::ListSpaces => "matrix.spaces.list",
            ReadToolKind::SpaceInfo { .. } => "matrix.space.info.read",
            ReadToolKind::SpaceRooms { .. } => "matrix.space.rooms.list",
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
/// [`AI_ROOM_SESSION_CAP_IDS`], which adds the generator and the app tools).
/// Kept beside the tool impls so the tool list and the runtime's gate cannot
/// disagree. The native `send_message` tool is room plumbing, not a catalog
/// capability, so it is deliberately not listed anywhere here — and neither
/// is `read_room_memory`, which is the same kind of ungated plumbing (the
/// agent recalling its own past turns).
pub const AI_ROOM_READ_CAP_IDS: &[&str] = &[
    "matrix.room.messages.read",
    "matrix.room.messages.paginate",
    "matrix.room.info.read",
    "matrix.rooms.messages.read",
    "matrix.rooms.list",
    "matrix.spaces.list",
    "matrix.space.info.read",
    "matrix.space.rooms.list",
];

/// Everything an AI session may do that sits in the mini-app capability
/// catalog: the read tools above, the generator (`launch_splash_app`), the
/// app tools (`list_apps`, `launch_app`), and posting into the user's other
/// rooms (`post_room_message` — gated per room, see the runtime). What the
/// runtime's gate checks against, so every offered capability is declared —
/// a profile list can only grow by editing this array.
pub const AI_ROOM_SESSION_CAP_IDS: &[&str] = &[
    "matrix.room.messages.read",
    "matrix.room.messages.paginate",
    "matrix.room.info.read",
    "matrix.rooms.messages.read",
    "matrix.rooms.list",
    "matrix.spaces.list",
    "matrix.space.info.read",
    "matrix.space.rooms.list",
    "apps.generate",
    "apps.list",
    "apps.launch",
    "matrix.rooms.message.send",
    // The AI room can invoke tools a mini-app registered in it (each tool is
    // granted per tool, see `PermissionStore::tool_effective`); declaring the
    // incoming hook puts the `mcp-tools` group in the room's AI panel so the
    // user can see and revoke it there.
    "on_tool_call",
    // Internet access (the agent's own web_search / web_fetch / browser
    // tools), gated per host so nothing reaches the network until the user
    // has allowed that host for this room's AI.
    "network.http",
];

/// The catalog capability behind `list_apps`; the runtime gates the tool
/// against it. Kept beside the session profile so the tool list and the gate
/// cannot disagree (mirrors [`ReadToolKind::capability_id`]).
pub const LIST_APPS_CAP_ID: &str = "apps.list";

/// The catalog capability behind `launch_app`; the runtime gates the tool
/// against it (see [`LIST_APPS_CAP_ID`]).
pub const LAUNCH_APP_CAP_ID: &str = "apps.launch";

/// The MCP tool name for a read kind — what shows on the room's `ai_reply`
/// receipt chip.
pub fn read_tool_name(kind: &ReadToolKind) -> &'static str {
    match kind {
        ReadToolKind::Messages { .. } => "read_room_messages",
        ReadToolKind::Older { .. } => "read_older_messages",
        ReadToolKind::Info => "room_info",
        ReadToolKind::OtherRoom { .. } => "read_other_room_messages",
        ReadToolKind::ListRooms => "list_rooms",
        ReadToolKind::ListSpaces => "list_spaces",
        ReadToolKind::SpaceInfo { .. } => "space_info",
        ReadToolKind::SpaceRooms { .. } => "list_space_rooms",
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
         yet). The user is asked to allow each room the first time you read \
         from it; once allowed that one room stays allowed, but a different \
         room is a fresh ask. Use list_rooms to see which rooms exist."
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

/// `list_spaces` — the joined spaces, with names and ids. Gated by
/// `matrix.spaces.list`; the user is asked on first use.
pub struct ListSpacesTool {
    host: Arc<dyn AiHost>,
}

impl ListSpacesTool {
    pub fn new(host: Arc<dyn AiHost>) -> Self {
        Self { host }
    }
}

impl Tool for ListSpacesTool {
    fn name(&self) -> &str {
        "list_spaces"
    }

    fn description(&self) -> &str {
        "Lists the spaces you have joined: space id, name, topic and member \
         count. A space groups rooms, so this is the map to look at before \
         drilling into one with list_space_rooms or space_info."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "additionalProperties": false })
    }

    fn call(&self, _arguments: &Map<String, Value>) -> Result<String, String> {
        self.host.read_tool(ReadToolKind::ListSpaces)
    }
}

/// `space_info` — one named space's cheap public facts. Gated by
/// `matrix.space.info.read`; the user is asked on first use.
pub struct SpaceInfoTool {
    host: Arc<dyn AiHost>,
}

impl SpaceInfoTool {
    pub fn new(host: Arc<dyn AiHost>) -> Self {
        Self { host }
    }
}

impl Tool for SpaceInfoTool {
    fn name(&self) -> &str {
        "space_info"
    }

    fn description(&self) -> &str {
        "Returns one space's details: name, topic, member count, join rule, \
         whether it is world-readable, and how many child rooms/subspaces it \
         has. Pass the space id from list_spaces."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "space": {
                    "type": "string",
                    "description": "The space (matrix room) id to inspect, e.g. !abc:server.org (see list_spaces).",
                },
            },
            "required": ["space"],
            "additionalProperties": false,
        })
    }

    fn call(&self, arguments: &Map<String, Value>) -> Result<String, String> {
        let space = space_arg(arguments)?;
        self.host.read_tool(ReadToolKind::SpaceInfo { space })
    }
}

/// `list_space_rooms` — the child rooms and subspaces of one named space.
/// Gated by `matrix.space.rooms.list`; the user is asked on first use.
pub struct ListSpaceRoomsTool {
    host: Arc<dyn AiHost>,
}

impl ListSpaceRoomsTool {
    pub fn new(host: Arc<dyn AiHost>) -> Self {
        Self { host }
    }
}

impl Tool for ListSpaceRoomsTool {
    fn name(&self) -> &str {
        "list_space_rooms"
    }

    fn description(&self) -> &str {
        "Lists the rooms and subspaces inside one space: name, room id, \
         topic, whether it is a space, whether you are joined, member count \
         and join rule. Use list_spaces to find the space id, then this to \
         see the rooms it groups."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "space": {
                    "type": "string",
                    "description": "The space (matrix room) id to list the rooms of, e.g. !abc:server.org (see list_spaces).",
                },
            },
            "required": ["space"],
            "additionalProperties": false,
        })
    }

    fn call(&self, arguments: &Map<String, Value>) -> Result<String, String> {
        let space = space_arg(arguments)?;
        self.host.read_tool(ReadToolKind::SpaceRooms { space })
    }
}

/// The required `space` id argument shared by the space tools, trimmed and
/// refused when absent.
fn space_arg(arguments: &Map<String, Value>) -> Result<String, String> {
    arguments
        .get("space")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "this tool needs a `space` id".to_string())
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
        "Builds a NEW sandboxed mini-app from a natural-language description and \
         installs it into this room, then runs it. Call this when the user asks \
         to create, build, or make a brand-new app. It always creates a new app; \
         it never changes one that already exists. To run an app the user \
         already has, use list_apps and launch_app. The result is a JSON summary \
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

/// `list_apps` — the installed mini-apps available in this room.
///
/// Read-only metadata (id, name, description, scope, running) so the model can
/// name one to `launch_app`; it never exposes an app's source or data.
pub struct ListAppsTool {
    host: Arc<dyn AiHost>,
}

impl ListAppsTool {
    pub fn new(host: Arc<dyn AiHost>) -> Self {
        Self { host }
    }
}

impl Tool for ListAppsTool {
    fn name(&self) -> &str {
        "list_apps"
    }

    fn description(&self) -> &str {
        "Lists the mini-apps installed and available in this room, with each \
         app's `id`, name, description, whether it is scoped to this room or the \
         whole account, and whether it is currently running. Call this when the \
         user asks to open, run, or start an app they already have, then pass \
         the `id` to launch_app. This only lists apps; to write a new one, use \
         launch_splash_app."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })
    }

    fn call(&self, _arguments: &Map<String, Value>) -> Result<String, String> {
        self.host.list_apps()
    }
}

/// `launch_app` — run an already-installed mini-app in this room, by id.
///
/// The counterpart to `launch_splash_app`: it never generates, rewrites, or
/// installs. The id must come from `list_apps`; an unknown or unavailable id is
/// an error the model can read and recover from.
pub struct LaunchAppTool {
    host: Arc<dyn AiHost>,
}

impl LaunchAppTool {
    pub fn new(host: Arc<dyn AiHost>) -> Self {
        Self { host }
    }
}

impl Tool for LaunchAppTool {
    fn name(&self) -> &str {
        "launch_app"
    }

    fn description(&self) -> &str {
        "Runs an already-installed mini-app in this room, identified by the `id` \
         from list_apps. It opens the app in this room's dock. This never \
         creates or changes an app — to build a new one, use launch_splash_app."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "app_id": {
                    "type": "string",
                    "description": "The id of an installed app, exactly as returned by list_apps.",
                },
            },
            "required": ["app_id"],
            "additionalProperties": false,
        })
    }

    fn call(&self, arguments: &Map<String, Value>) -> Result<String, String> {
        let app_id = arguments
            .get("app_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "`launch_app` needs a non-empty string `app_id`".to_string())?
            .to_string();
        self.host.launch_app(&app_id)
    }
}

/// `list_mini_app_tools` — the tools the room's mini-apps offer the agent.
///
/// Sibling of `list_apps`: that one names the apps, this one names the
/// callable tools inside them. It exists because an agent discovers its MCP
/// tools once, at session start, so a tool an app registers later can never
/// appear in that snapshot on its own — the model reaches it through the
/// stable `call_mini_app_tool` instead, after reading this list.
///
/// Read-only metadata (id, name, description, arguments): it never exposes an
/// app's source or data, and the invocation itself is still gated per tool.
pub struct ListMiniAppToolsTool {
    host: Arc<dyn AiHost>,
}

impl ListMiniAppToolsTool {
    pub fn new(host: Arc<dyn AiHost>) -> Self {
        Self { host }
    }
}

impl Tool for ListMiniAppToolsTool {
    fn name(&self) -> &str {
        "list_mini_app_tools"
    }

    fn description(&self) -> &str {
        "Lists the tools the mini-apps in this room have registered for you to \
         call, with each tool's `tool` id, its name, description, and \
         arguments. A mini-app can offer tools at runtime, so its own list can \
         change while you talk to it. Call this whenever a mini-app tells you \
         it has a tool, then pass the `tool` id to call_mini_app_tool. This \
         only lists; nothing is invoked."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })
    }

    fn call(&self, _arguments: &Map<String, Value>) -> Result<String, String> {
        self.host.list_mini_app_tools()
    }
}

/// `call_mini_app_tool` — invoke a tool one of the room's mini-apps registered.
///
/// The stable counterpart to `list_mini_app_tools`: a session advertises this
/// from the start, so the model can call a tool that was registered after the
/// agent connected. The runtime validates the id, applies the same per-tool
/// permission gate as a direct call, and routes the invocation into the
/// owning isolate.
pub struct CallMiniAppToolTool {
    host: Arc<dyn AiHost>,
}

impl CallMiniAppToolTool {
    pub fn new(host: Arc<dyn AiHost>) -> Self {
        Self { host }
    }
}

impl Tool for CallMiniAppToolTool {
    fn name(&self) -> &str {
        "call_mini_app_tool"
    }

    fn description(&self) -> &str {
        "Calls a tool a mini-app in this room registered, using a `tool` id \
         from list_mini_app_tools and passing `arguments` as a JSON object \
         matching that tool's arguments. Use it to act inside an app — play a \
         game move, update its state, read what it holds. The result is the \
         app's own answer. If you do not know the tool ids or their \
         arguments, call list_mini_app_tools first."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "tool": {
                    "type": "string",
                    "description": "The `tool` id from list_mini_app_tools (or the app's own name for it).",
                },
                "arguments": {
                    "type": "object",
                    "description": "Arguments for the tool, matching its declared arguments.",
                },
            },
            "required": ["tool"],
            "additionalProperties": false,
        })
    }

    fn call(&self, arguments: &Map<String, Value>) -> Result<String, String> {
        let tool = arguments
            .get("tool")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| "`call_mini_app_tool` needs a non-empty string `tool`".to_string())?
            .to_string();
        let tool_arguments = match arguments.get("arguments") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(map)) => map.clone(),
            Some(_) => return Err("`arguments` must be a JSON object".to_string()),
        };
        self.host.call_mini_app_tool(&tool, tool_arguments)
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
    server.add_tool(ListSpacesTool::new(host.clone()));
    server.add_tool(SpaceInfoTool::new(host.clone()));
    server.add_tool(ListSpaceRoomsTool::new(host.clone()));
    server.add_tool(LaunchSplashAppTool::new(host.clone()));
    server.add_tool(ListAppsTool::new(host.clone()));
    server.add_tool(LaunchAppTool::new(host.clone()));
    // The stable bridge to tools mini-apps register at runtime: an agent that
    // learned its tool list once can still discover and call them.
    server.add_tool(ListMiniAppToolsTool::new(host.clone()));
    server.add_tool(CallMiniAppToolTool::new(host.clone()));
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
            (ReadToolKind::ListSpaces, "matrix.spaces.list"),
            (ReadToolKind::SpaceInfo { space: "!s:s".to_string() }, "matrix.space.info.read"),
            (ReadToolKind::SpaceRooms { space: "!s:s".to_string() }, "matrix.space.rooms.list"),
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
            ListSpaces,
            SpaceInfo { space: "!s:s".to_string() },
            SpaceRooms { space: "!s:s".to_string() },
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

    /// The app tools (`list_apps`, `launch_app`) map onto declared, gateable
    /// capabilities in the session profile — so neither can be offered to a
    /// model without a permission prompt behind it.
    #[test]
    fn app_tools_are_gated_by_declared_capabilities() {
        for id in [LIST_APPS_CAP_ID, LAUNCH_APP_CAP_ID] {
            assert!(
                AI_ROOM_SESSION_CAP_IDS.contains(&id),
                "{id} is offered as a tool but not in AI_ROOM_SESSION_CAP_IDS"
            );
            let cap = by_id(id).unwrap_or_else(|| panic!("{id} dropped from the catalog"));
            assert!(cap.is_available(), "{id} not available");
            assert_eq!(cap.group.map(|g| g.as_str()), Some("app-launch"), "{id} group");
        }
    }

    /// Granting “read this room” must NOT unlock `read_other_room_messages`:
    /// the two are separate permission groups, so the session must still be
    /// prompted the first time it reads a room outside its own. This guards the
    /// distinction the runtime's gate depends on.
    #[test]
    fn this_room_read_does_not_grant_other_rooms() {
        use a2app_core::permissions::{
            agent_subject, Effective, GrantState, Permission, PermissionStore,
        };
        let subject = agent_subject("!room:server.org");
        let declares_perm = |p: Permission| {
            AI_ROOM_SESSION_CAP_IDS
                .iter()
                .any(|id| by_id(id).and_then(|c| c.group).is_some_and(|g| g == p))
        };
        let declares_cap =
            |c: &a2app_core::capabilities::Capability| AI_ROOM_SESSION_CAP_IDS.contains(&c.id);
        let mut store = PermissionStore::default();
        store.set(&subject, Permission::MatrixRoomRead, GrantState::Granted);
        assert_eq!(
            store.effective_capability_for(
                &subject,
                declares_perm,
                declares_cap,
                by_id("matrix.room.messages.read").unwrap()
            ),
            Effective::Granted,
            "reading this room's messages should be allowed"
        );
        assert_eq!(
            store.effective_capability_for(
                &subject,
                declares_perm,
                declares_cap,
                by_id("matrix.rooms.messages.read").unwrap()
            ),
            Effective::NeedsPrompt,
            "reading another room must still prompt"
        );
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

    /// A mini-app tool is the model-facing name plus the app's own description
    /// and schema, and a call is forwarded verbatim to the bridge (which
    /// routes it into the isolate). This is the protocol half of the bridge,
    /// independent of any socket.
    #[test]
    fn a_miniapp_tool_forwards_its_name_and_arguments() {
        #[derive(Default)]
        struct RecordingBridge {
            calls: std::sync::Mutex<Vec<(String, Map<String, Value>)>>,
        }
        impl MiniAppToolBridge for RecordingBridge {
            fn invoke(&self, tool: &str, arguments: &Map<String, Value>) -> Result<String, String> {
                self.calls.lock().unwrap().push((tool.to_string(), arguments.clone()));
                Ok(json!({ "board": ["X", "", "O"], "turn": "X" }).to_string())
            }
        }

        let bridge = Arc::new(RecordingBridge::default());
        let schema = json!({
            "type": "object",
            "properties": {"cell": {"type": "integer"}},
            "required": ["cell"],
        });
        let tool = MiniAppTool::new(
            "app_board_play",
            "Place a mark on the board.",
            schema.clone(),
            bridge.clone(),
        );
        assert_eq!(tool.name(), "app_board_play");
        assert_eq!(tool.description(), "Place a mark on the board.");
        assert_eq!(tool.input_schema(), schema);

        let mut args = Map::new();
        args.insert("cell".into(), json!(4));
        let out = tool.call(&args).unwrap();
        assert!(out.contains("board"));
        let calls = bridge.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "app_board_play");
        assert_eq!(calls[0].1["cell"], json!(4));
    }
}
