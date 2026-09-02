//! Minimal Model Context Protocol (MCP) **server** support for Robrix's
//! host-tool bridge.
//!
//! Why this exists: an ACP agent (`octos acp`, `claude-code-acp`) can only
//! call host-provided tools by talking to an MCP *server* — octos through the
//! `mcp_servers` in its config, Claude Code through `mcpServers` at
//! `session/new`. Both spawn a `robrix --mcp-bridge` child whose stdio speaks
//! exactly this protocol, and relay its frames to the real Robrix process over
//! a session-scoped socket. This module is the protocol half of that server:
//! the framing (what rmcp — the SDK inside octos — actually sends over stdio)
//! and the request dispatch over a registry of tools.
//!
//! Deliberately dependency-light, in the style of [`crate::acp_client`]: plain
//! `serde_json` over newline-delimited JSON-RPC, no async runtime. The agent
//! side already proved that shape — one thread reads frames, a pure function
//! turns each into reply frames, another thread writes them.
//!
//! # Wire format
//!
//! MCP's stdio transport is one JSON-RPC message per line (CRLF tolerated,
//! blank lines skipped) — verified against `rmcp` 1.8, which is the SDK
//! octos's MCP client is built on (`rmcp-1.8.0/src/transport/async_rw.rs`
//! reads with `read_until(b'\n')`). No Content-Length framing.
//!
//! # Adding a tool
//!
//! Register one [`Tool`] on the [`McpServer`]. That is the whole surface: the
//! tool's `name`, `description` and `input_schema` are what the agent's model
//! sees (and what decides whether it calls it), and `call` is what runs in
//! Robrix. The Robrix side provides the implementations — app generation, room
//! messaging, whatever comes next — through the same registry.

use std::io::{BufRead, Read, Write};
use std::sync::Arc;

use serde_json::{json, Map, Value};

/// The MCP spec version this bridge speaks when a client asks for something
/// newer than we support. `rmcp` 1.8's own `LATEST`.
pub const PROTOCOL_VERSION_LATEST: &str = "2025-11-25";

/// Every MCP protocol version this bridge can answer an `initialize` for.
/// Older rmcp clients and other MCP SDKs send older dates; each is accepted
/// and echoed back so the client stays on whatever it already understands.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[
    "2024-11-05",
    "2025-03-26",
    "2025-06-18",
    PROTOCOL_VERSION_LATEST,
];

/// The name Robrix's tool server identifies itself as in `initialize`.
/// Matches the `name` field agents are configured with (`robrix-tools`).
pub const SERVER_NAME: &str = "robrix-tools";

/// Cap on one incoming MCP frame. Real frames are a few KB at most (the
/// largest are `tools/list` replies carrying every tool's JSON schema); a
/// frame past this is not a protocol we can answer anyway, so the reader
/// treats it as a broken peer rather than buffering without bound. The bridge
/// relay uses a far larger cap, since it forwards whatever the agent sends.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Reads one `\n`-terminated frame into `buf` (cleared first), tolerating a
/// trailing `\r` (CRLF) and skipping nothing else. Returns false on EOF with
/// nothing read. A frame hitting `cap` bytes is returned as-is; callers check
/// `buf.len() >= cap` to detect the oversize case.
///
/// This is `acp_client::read_capped_line`'s discipline, kept in lockstep so a
/// wedged or malicious peer can neither wedge the reader nor balloon memory.
pub fn read_frame(reader: &mut impl BufRead, cap: usize, buf: &mut Vec<u8>) -> bool {
    buf.clear();
    let mut limited = reader.take(cap as u64);
    match limited.read_until(b'\n', buf) {
        Ok(0) => false,
        Ok(_) => {
            while buf.last() == Some(&b'\n') || buf.last() == Some(&b'\r') {
                buf.pop();
            }
            true
        }
        Err(_) => false,
    }
}

/// Forwards frames from `from` to `to` verbatim until EOF.
///
/// The `--mcp-bridge` relay process is exactly this: it must not parse,
/// reorder or rewrite anything, because the two ends (the agent's MCP client
/// and Robrix's tool server) negotiate between themselves. Bytes are copied
/// through the terminating newline so CRLF frames pass untouched. Returns when
/// the source closes (EOF at a frame boundary) — a peer that dies mid-frame
/// still gets its partial bytes forwarded before we stop.
pub fn copy_frames(from: &mut impl BufRead, to: &mut impl Write) -> std::io::Result<()> {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = from.read_until(b'\n', &mut buf)?;
        if n == 0 {
            return Ok(());
        }
        to.write_all(&buf)?;
        to.flush()?;
        if !buf.ends_with(b"\n") {
            // EOF landed mid-frame; the partial frame went out, nothing more
            // is coming.
            return Ok(());
        }
    }
}

/// A stdio MCP server an ACP agent should be told about when its session
/// starts — expressed the way each agent's config wants it (Claude Code's
/// `mcpServers` array at `session/new`; octos's `mcp_servers` config array).
///
/// Robrix registers ITSELF as the server: `command` is the Robrix binary and
/// `args` carry `--mcp-bridge --socket <path>`, so when the model decides to
/// call a tool the agent spawns a dumb relay child whose stdio reaches back
/// into this process's session-scoped tool server (the app's `a2app::ai`).
/// Building the entry for a live socket is the app's job; this is just the
/// shape the ACP handshake needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerConfig {
    /// The server's display name; the agent's tool list is grouped under it.
    pub name: String,
    /// The binary the agent spawns as its stdio MCP server.
    pub command: String,
    /// Its arguments (e.g. `--mcp-bridge --socket <path>`).
    pub args: Vec<String>,
}

impl McpServerConfig {
    pub fn new(name: impl Into<String>, command: impl Into<String>, args: Vec<String>) -> Self {
        Self { name: name.into(), command: command.into(), args }
    }
}

/// A host tool the agent's model can call.
///
/// The registry treats implementations as opaque: Robrix registers a concrete
/// tool per capability (generate a mini-app, send a room message, …), and the
/// protocol layer here only ever sees the four things below.
pub trait Tool: Send + Sync {
    /// The tool's name as the model addresses it (`tools/call`'s `name`).
    fn name(&self) -> &str;

    /// What the tool does, for the model. MCP servers are expected to say how
    /// to use a tool here, since models read it when deciding whether to call.
    fn description(&self) -> &str;

    /// A JSON Schema object for the tool's `arguments`, declared to the agent
    /// in `tools/list`. octos bounds this: at most 10 levels deep and 64 KB
    /// serialized, or the tool is silently skipped.
    fn input_schema(&self) -> Value;

    /// Executes the tool with the caller's `arguments`.
    ///
    /// `Err` becomes an MCP `isError: true` result whose text is the error —
    /// that is what reaches the model as the tool's message (octos folds text
    /// content into the agent turn and reads `isError` to mark the call
    /// failed). Implementations decide how long they may block: octos cancels
    /// a `tools/call` that runs past 60 seconds, so a tool that genuinely
    /// takes minutes (app generation) must either be quick or be structured as
    /// start-then-poll — that constraint lives with the implementation, not
    /// the protocol.
    fn call(&self, arguments: &Map<String, Value>) -> Result<String, String>;
}

/// A parsed inbound request or notification, reduced from raw JSON.
enum Inbound {
    /// A request that expects a reply (has an id). `method` is non-empty.
    Request { id: Value, method: String, params: Value },
    /// A notification (no id) — the client is telling us something, nothing
    /// comes back.
    Notification { method: String },
    /// A reply to a request we never sent, or a frame we can't make sense of.
    /// Nothing to do with it.
    Ignore,
}

/// The MCP tool server state for one connection.
///
/// One of these serves one agent's connection to Robrix. It is a pure state
/// machine over frames: feed it each inbound line with [`Self::handle_frame`]
/// and write back whatever frames it returns (newline-terminated). It never
/// touches I/O itself, which is what makes the dispatch testable without a
/// socket and reusable by whichever thread owns the connection.
///
/// Cheap to clone: the tool set is shared (`Arc`), so a listener can clone a
/// template per connection and hand each one to its own thread.
#[derive(Clone)]
pub struct McpServer {
    tools: Vec<Arc<dyn Tool>>,
}

impl McpServer {
    /// An empty server; register tools with [`Self::add_tool`] before (or
    /// after) the connection starts — `tools/list` reflects whatever is
    /// registered when it arrives.
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    /// Adds one tool to the registry. Adding a host tool is this call plus the
    /// [`Tool`] implementation; nothing else in Robrix or the protocol layer
    /// changes.
    pub fn add_tool(&mut self, tool: impl Tool + 'static) {
        self.tools.push(Arc::from(tool));
    }

    /// The registered tools, in registration order — what `tools/list` will
    /// advertise.
    pub fn tools(&self) -> impl Iterator<Item = &dyn Tool> {
        self.tools.iter().map(|t| t.as_ref())
    }

    /// Handles one inbound frame (a single line, newline already stripped) and
    /// returns the reply frames to send, each WITHOUT its trailing newline.
    /// Returns an empty vec for notifications and for frames that need no
    /// reply.
    pub fn handle_frame(&self, line: &str) -> Vec<String> {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            // Not JSON at all. The protocol has no way to answer an unparseable
            // frame (we may not even have an id), and a real client never sends
            // one — the reader's cap already filtered garbage out. Drop it.
            return Vec::new();
        };
        match parse_inbound(&value) {
            Inbound::Ignore => Vec::new(),
            Inbound::Notification { method } => {
                // `notifications/initialized`, `notifications/cancelled`,
                // `notifications/progress`, … — acknowledge by silence. There
                // is nothing a server must do for any of them here (progress
                // notifications only matter for requests we initiated, which
                // we never do).
                let _ = method;
                Vec::new()
            }
            Inbound::Request { id, method, params } => {
                vec![self.dispatch(&id, &method, &params)]
            }
        }
    }

    fn dispatch(&self, id: &Value, method: &str, params: &Value) -> String {
        let result = match method {
            "initialize" => self.initialize(params),
            "ping" => Ok(json!({})),
            "tools/list" => self.list_tools(),
            "tools/call" => self.call_tool(params),
            // Everything else — resources/*, prompts/*, logging/*, anything a
            // future protocol version adds — is a method this server does not
            // implement. Answering -32601 (rather than silence) lets the
            // client's own machinery surface the gap instead of hanging.
            _ => Err(McpError::method_not_found(method)),
        };
        reply(id, result)
    }

    /// The `initialize` reply. The protocol-version answer follows MCP's
    /// negotiation: echo the client's version when it is one we support;
    /// otherwise offer our own newest (a client asking for something newer
    /// downgrades to what we answer).
    fn initialize(&self, params: &Value) -> Result<Value, McpError> {
        let requested = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or("");
        let version = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
            requested
        } else {
            PROTOCOL_VERSION_LATEST
        };
        Ok(json!({
            "protocolVersion": version,
            "capabilities": {
                // Tools are the whole surface. `listChanged: false` promises
                // no `notifications/tools/list_changed` — the tool set is
                // static for the life of a session.
                "tools": {"listChanged": false},
            },
            "serverInfo": {
                "name": SERVER_NAME,
                "version": env!("CARGO_PKG_VERSION"),
            },
        }))
    }

    fn list_tools(&self) -> Result<Value, McpError> {
        let tools: Vec<Value> = self
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name(),
                    "description": tool.description(),
                    // The schema must be a JSON object (an empty one means
                    // "no arguments").
                    "inputSchema": tool.input_schema(),
                })
            })
            .collect();
        // No `nextCursor`: rmcp's list_all_tools loops on a cursor, and a
        // server that keeps handing one out gets paged to death.
        Ok(json!({ "tools": tools }))
    }

    fn call_tool(&self, params: &Value) -> Result<Value, McpError> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| McpError::invalid_params("tools/call needs a `name`"))?;
        let tool = self
            .tools
            .iter()
            .find(|tool| tool.name() == name)
            .ok_or_else(|| McpError::invalid_params(format!("unknown tool `{name}`")))?;
        // `arguments` is optional and must be an object; anything else is a
        // caller bug worth surfacing as a JSON-RPC error, not a tool failure.
        let arguments = match params.get("arguments") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(map)) => map.clone(),
            Some(_) => {
                return Err(McpError::invalid_params(format!(
                    "tool `{name}` arguments must be an object"
                )));
            }
        };
        match tool.call(&arguments) {
            // The model reads text content; octos joins all text parts and
            // drops non-text ones, so one text block is the whole message.
            Ok(text) => Ok(json!({
                "content": [{"type": "text", "text": text}],
                "isError": false,
            })),
            // A tool failure is NOT a JSON-RPC error: isError keeps the reason
            // inside the result, where the agent turns it into a model-visible
            // message. A JSON-RPC error would surface as a transport failure
            // with the reason buried.
            Err(message) => Ok(json!({
                "content": [{"type": "text", "text": message}],
                "isError": true,
            })),
        }
    }
}

impl Default for McpServer {
    fn default() -> Self {
        Self::new()
    }
}

/// Splits a raw JSON-RPC value into what the server should do with it.
fn parse_inbound(value: &Value) -> Inbound {
    // Notifications and requests both carry `method`; a reply (to a request we
    // never sent) carries `result`/`error` instead.
    let Some(method) = value.get("method").and_then(Value::as_str) else {
        return Inbound::Ignore;
    };
    let has_id = value.get("id").is_some_and(|id| !id.is_null());
    let params = value.get("params").cloned().unwrap_or_else(|| json!({}));
    if has_id {
        Inbound::Request { id: value.get("id").cloned().unwrap_or(Value::Null), method: method.to_string(), params }
    } else {
        Inbound::Notification { method: method.to_string() }
    }
}

/// Wraps a dispatch result in a JSON-RPC reply frame. Ids are echoed verbatim
/// (JSON-RPC allows numbers or strings; rmcp sends numbers).
fn reply(id: &Value, result: Result<Value, McpError>) -> String {
    let body = match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(err) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": err.into_json() })
        }
    };
    body.to_string()
}

/// A JSON-RPC error reply. These are for protocol-level failures (unknown
/// method, malformed parameters); failures *of a tool* go back as `isError`
/// results instead so the model sees the reason.
struct McpError {
    code: i64,
    message: String,
}

impl McpError {
    /// -32601: the method exists in neither this server nor this protocol.
    fn method_not_found(method: &str) -> Self {
        Self { code: -32601, message: format!("method not found: {method}") }
    }

    /// -32602: the request's parameters are wrong (unknown tool, bad shape).
    fn invalid_params(message: impl Into<String>) -> Self {
        Self { code: -32602, message: message.into() }
    }

    fn into_json(self) -> Value {
        json!({ "code": self.code, "message": self.message })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A canned tool that echoes its arguments as text, so tests can assert
    /// the full call round trip through the JSON-RPC layer.
    struct EchoTool;

    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echoes back its arguments as text, for tests."
        }
        fn input_schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
            })
        }
        fn call(&self, arguments: &Map<String, Value>) -> Result<String, String> {
            Ok(arguments
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string())
        }
    }

    /// A tool that always fails, so tests can assert the isError path.
    struct FailingTool;

    impl Tool for FailingTool {
        fn name(&self) -> &str {
            "boom"
        }
        fn description(&self) -> &str {
            "Always fails."
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn call(&self, _arguments: &Map<String, Value>) -> Result<String, String> {
            Err("kaboom".to_string())
        }
    }

    fn server() -> McpServer {
        let mut server = McpServer::new();
        server.add_tool(EchoTool);
        server.add_tool(FailingTool);
        server
    }

    /// Parses one JSON-RPC reply frame back into (id, result | error).
    fn reply_of(line: &str) -> (Value, Result<Value, String>) {
        let value: Value = serde_json::from_str(line).expect("valid JSON-RPC reply");
        let id = value.get("id").cloned().unwrap_or(Value::Null);
        if let Some(result) = value.get("result") {
            (id, Ok(result.clone()))
        } else {
            let msg = value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();
            (id, Err(msg))
        }
    }

    /// The exact handshake an rmcp-based client (octos) runs:
    /// initialize → initialized notification → tools/list → tools/call.
    #[test]
    fn an_rmcp_client_session_round_trips() {
        let server = server();

        // initialize: newest known version is echoed.
        let replies = server.handle_frame(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2025-11-25",
                "capabilities":{},
                "clientInfo":{"name":"octos","version":"test"}
            }}"#,
        );
        assert_eq!(replies.len(), 1);
        let (id, result) = reply_of(&replies[0]);
        assert_eq!(id, json!(1));
        let result = result.unwrap();
        assert_eq!(result["protocolVersion"], "2025-11-25");
        assert_eq!(result["serverInfo"]["name"], SERVER_NAME);
        assert_eq!(result["capabilities"]["tools"]["listChanged"], false);

        // An older client's version is echoed back, not bumped.
        let replies = server.handle_frame(
            r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{
                "protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{}
            }}"#,
        );
        let (_, result) = reply_of(&replies[0]);
        assert_eq!(result.unwrap()["protocolVersion"], "2024-11-05");

        // A version newer than we support gets our newest, and the client
        // downgrades to it.
        let replies = server.handle_frame(
            r#"{"jsonrpc":"2.0","id":3,"method":"initialize","params":{
                "protocolVersion":"2999-01-01","capabilities":{},"clientInfo":{}
            }}"#,
        );
        let (_, result) = reply_of(&replies[0]);
        assert_eq!(result.unwrap()["protocolVersion"], PROTOCOL_VERSION_LATEST);

        // The initialized notification (no id) is answered with silence.
        assert!(
            server
                .handle_frame(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .is_empty()
        );

        // tools/list advertises both registered tools with their schemas, and
        // no pagination cursor (rmcp loops while one is present).
        let replies = server.handle_frame(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/list","params":{}}"#,
        );
        assert_eq!(replies.len(), 1);
        let (_, result) = reply_of(&replies[0]);
        let result = result.unwrap();
        assert!(result.get("nextCursor").is_none(), "no cursor, ever");
        let names: Vec<&str> = result["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["echo", "boom"]);
        assert_eq!(result["tools"][0]["inputSchema"]["type"], "object");

        // A successful call returns the tool's text as content, isError false.
        let replies = server.handle_frame(
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{
                "name":"echo","arguments":{"text":"hi there"}
            }}"#,
        );
        let (id, result) = reply_of(&replies[0]);
        assert_eq!(id, json!(5));
        let result = result.unwrap();
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][0]["text"], "hi there");
    }

    /// A tool that fails reports through isError (model-visible), not as a
    /// JSON-RPC error (transport-invisible).
    #[test]
    fn a_tool_failure_is_an_iserror_result() {
        let server = server();
        let replies = server.handle_frame(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{
                "name":"boom","arguments":{}
            }}"#,
        );
        let (_, result) = reply_of(&replies[0]);
        let result = result.unwrap();
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"][0]["text"], "kaboom");
    }

    /// Protocol errors are JSON-RPC errors: an unknown method, an unknown
    /// tool, or arguments that aren't an object.
    #[test]
    fn protocol_errors_are_jsonrpc_errors() {
        let server = server();

        let replies = server.handle_frame(
            r#"{"jsonrpc":"2.0","id":8,"method":"resources/list","params":{}}"#,
        );
        let (_, err) = reply_of(&replies[0]);
        assert!(err.unwrap_err().contains("resources/list"));

        let replies = server.handle_frame(
            r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{
                "name":"nope","arguments":{}
            }}"#,
        );
        let (_, err) = reply_of(&replies[0]);
        assert!(err.unwrap_err().contains("unknown tool `nope`"));

        let replies = server.handle_frame(
            r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{
                "name":"echo","arguments":"not an object"
            }}"#,
        );
        assert!(reply_of(&replies[0]).1.is_err());
    }

    /// Notifications (cancelled, progress, …) and replies to requests we never
    /// sent are ignored, and garbage lines never produce a reply.
    #[test]
    fn notifications_and_noise_are_silent() {
        let server = server();
        for line in [
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{}}"#,
            // A reply to a request this server never sent.
            r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#,
            "this is not json at all",
        ] {
            assert!(server.handle_frame(line).is_empty(), "unexpected reply to {line}");
        }
    }

    /// Framing: CRLF frames are tolerated, EOF mid-frame still forwards, and
    /// a cap is enforced by the caller.
    #[test]
    fn frames_read_and_forward_verbatim() {
        let mut input = std::io::Cursor::new(
            b"{\"a\":1}\r\n{\"b\":2}\n{\"c\"".to_vec(), // last frame has no newline
        );
        let mut buf = Vec::new();
        assert!(read_frame(&mut input, MAX_FRAME_BYTES, &mut buf));
        assert_eq!(buf, b"{\"a\":1}");
        assert!(read_frame(&mut input, MAX_FRAME_BYTES, &mut buf));
        assert_eq!(buf, b"{\"b\":2}");
        // EOF in the middle of a frame: read_frame still hands back the bytes.
        assert!(read_frame(&mut input, MAX_FRAME_BYTES, &mut buf));
        assert_eq!(buf, b"{\"c\"");
        assert!(!read_frame(&mut input, MAX_FRAME_BYTES, &mut buf));
    }

    /// The relay is a verbatim pipe: frames in, identical bytes out, in order.
    #[test]
    fn copy_frames_passes_bytes_through_unchanged() {
        let mut input = std::io::Cursor::new(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n\
              {\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\r\n"
                .to_vec(),
        );
        let mut output = Vec::new();
        copy_frames(&mut input, &mut output).unwrap();
        assert_eq!(
            output,
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n\
              {\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\r\n"
        );
    }
}
