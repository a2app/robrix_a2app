//! End-to-end MCP transport smoke test.
//!
//! Topology under test — the exact shape an agent (octos, Claude Code) drives:
//!
//! ```text
//! this process (stands in for the real Robrix host)
//!   └─ ToolServer bound on a session socket, accept loop running
//!        ▲                                  │
//!        │ socket                           │ socket
//!        │                                  ▼
//!   robrix --mcp-bridge --socket <path>   (the relay child, spawned for real)
//!        ▲                                  │
//!        │ stdin                            │ stdout
//!        └── this process (also the agent's MCP client, hand-written) ──┘
//! ```
//!
//! The [`a2app_agent::mcp`] unit tests already prove the protocol half; this
//! proves the socket + relay halves together: the real binary intercepts
//! `--mcp-bridge` before any UI starts, its stdio↔socket relay forwards
//! frames verbatim in both directions, and the tools registered on the host
//! side come back through the whole pipe. No MCP client library is used — the
//! test writes JSON-RPC lines and reads reply lines itself.
//!
//! NOTE: these tests bind a Unix-domain socket, so they need an environment
//! that permits that (a normal terminal, CI). Seatbelt-sandboxed shells deny
//! AF_UNIX binds and make every case here fail with EPERM at bind time.

#![cfg(all(feature = "a2app", unix))]

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use a2app_agent::mcp::McpServer;
use robrix::a2app::ai::server::ToolServer;
use robrix::a2app::ai::tools::{AiHost, ReadToolKind, register_session_tools};
use serde_json::{Value, json};

/// How long a reply may take before the test gives up. A local socket round
/// trip is milliseconds; this is generous slack for a debug build under load.
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the relay child may take to exit after its session ends.
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);

/// A stand-in for the real Robrix-side [`AiHost`]: records every tool call so
/// the test can assert the model's arguments arrived intact, and returns the
/// canned payloads the real host would.
#[derive(Default)]
struct RecordingHost {
    calls: Mutex<Vec<String>>,
}

impl RecordingHost {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl AiHost for RecordingHost {
    fn launch_splash_app(&self, description: &str) -> Result<String, String> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("launch_splash_app({description:?})"));
        Ok(json!({
            "app_id": "smoke-app",
            "name": "Smoke App",
            "status": "installed_and_running",
        })
        .to_string())
    }

    fn send_room_message(&self, text: &str) -> Result<String, String> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("send_room_message({text:?})"));
        Ok("posted".to_string())
    }

    fn post_room_message(&self, room: &str, text: &str) -> Result<String, String> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("post_room_message({room:?}, {text:?})"));
        Ok("posted to the room as a notice".to_string())
    }

    fn read_tool(&self, _kind: ReadToolKind) -> Result<String, String> {
        // The transport tests never drive a read; record nothing, answer
        // as if the read were refused so a stray call is visible in `calls`.
        Err("read_tool not exercised by this test".to_string())
    }
}

/// Owns the relay child; kills it if the test ends (pass or panic) without
/// having reaped it, so no `robrix --mcp-bridge` strays survive a run.
struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn id(&self) -> u32 {
        self.0.as_ref().expect("child already taken").id()
    }

    fn take(&mut self) -> Child {
        self.0.take().expect("child already taken")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Reads `child.stdout` line by line on a background thread so the test can
/// wait on replies with a timeout instead of blocking forever on a dead relay.
fn spawn_line_reader(read: impl Read + Send + 'static) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(read);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break, // EOF or a dead peer: relay is gone.
                Ok(_) => {
                    if tx.send(line).is_err() {
                        break; // the test moved on; stop reading.
                    }
                }
            }
        }
    });
    rx
}

/// Waits for the child to exit, killing it after `timeout` (the caller then
/// fails on the `None`). The child is moved onto the waiter thread so a relay
/// that never exits cannot hang the test forever.
fn wait_for_exit(guard: &mut ChildGuard, timeout: Duration) -> Option<i32> {
    let pid = guard.id();
    let mut child = guard.take();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait());
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(status)) => status.code(),
        Ok(Err(e)) => panic!("waiting for the relay failed: {e}"),
        Err(_) => {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
            None
        }
    }
}

/// The hand-written MCP client driving the relay: JSON-RPC 2.0 over the
/// child's stdio, one frame per line — exactly what rmcp 1.8 does.
struct RelayClient {
    stdin: std::process::ChildStdin,
    lines: mpsc::Receiver<String>,
    next_id: u64,
}

impl RelayClient {
    /// Sends a request and returns its `result`. Fails the test on a JSON-RPC
    /// error or on a reply whose id doesn't echo ours (which would mean the
    /// relay reordered or invented frames).
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.write_frame(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let line = self
            .lines
            .recv_timeout(REPLY_TIMEOUT)
            .unwrap_or_else(|_| panic!("no reply to `{method}` within {REPLY_TIMEOUT:?}"));
        let reply: Value =
            serde_json::from_str(&line).unwrap_or_else(|_| panic!("reply is not JSON: {line}"));
        assert_eq!(reply["id"], json!(id), "reply id must echo the request id: {reply}");
        assert!(
            reply.get("error").is_none(),
            "unexpected JSON-RPC error for `{method}`: {reply}"
        );
        reply["result"].clone()
    }

    /// Like [`Self::request`] but returns the whole reply, error included —
    /// for asserting on JSON-RPC error paths (unknown tool, bad params).
    fn raw_request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.write_frame(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let line = self
            .lines
            .recv_timeout(REPLY_TIMEOUT)
            .unwrap_or_else(|_| panic!("no reply to `{method}` within {REPLY_TIMEOUT:?}"));
        let reply: Value =
            serde_json::from_str(&line).unwrap_or_else(|_| panic!("reply is not JSON: {line}"));
        assert_eq!(reply["id"], json!(id), "reply id must echo the request id: {reply}");
        reply
    }

    /// Sends a notification (no id, no reply expected).
    fn notify(&mut self, method: &str, params: Value) {
        self.write_frame(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Asserts that nothing arrives within `window`. The relay is a verbatim
    /// pipe, so any line here would be a frame the server actually sent.
    fn expect_silence(&self, window: Duration) {
        match self.lines.recv_timeout(window) {
            Ok(line) => panic!("expected silence, got a frame: {line}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("the relay died while silence was expected")
            }
        }
    }

    fn write_frame(&mut self, frame: &Value) {
        self.stdin
            .write_all(frame.to_string().as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
            .expect("write the frame to the relay's stdin");
    }
}

/// Binds a session tool server (this process = the real Robrix host) and
/// spawns `robrix --mcp-bridge` pointed at its socket.
fn host_and_relay() -> (ToolServer, ChildGuard, Arc<RecordingHost>) {
    let host: Arc<RecordingHost> = Arc::new(RecordingHost::default());
    let mut template = McpServer::new();
    register_session_tools(&mut template, host.clone());
    let server = ToolServer::bind(template).expect("bind the session socket");
    server.start().expect("start the accept loop");

    let child = Command::new(env!("CARGO_BIN_EXE_robrix"))
        .args(["--mcp-bridge", "--socket"])
        .arg(server.socket_path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Diagnostics from the relay child (usage errors, failed connects)
        // go to the test's own stderr.
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn `robrix --mcp-bridge`");

    (server, ChildGuard(Some(child)), host)
}

fn open_client(guard: &mut ChildGuard) -> RelayClient {
    let mut child = guard.take();
    let stdin = child.stdin.take().expect("relay stdin is piped");
    let stdout = child.stdout.take().expect("relay stdout is piped");
    let lines = spawn_line_reader(stdout);
    // The child is now owned by the guard again for reaping at the end.
    guard.0 = Some(child);
    RelayClient { stdin, lines, next_id: 0 }
}

/// The full session an MCP client runs against a Robrix tool server, driven
/// over a REAL `robrix --mcp-bridge` child: initialize → initialized
/// notification → tools/list → tools/call (success, validation failure,
/// unknown tool) → close stdin → relay exits 0.
#[test]
fn a_full_mcp_session_over_the_relay_child() {
    let (server, mut guard, host) = host_and_relay();
    let _server_kept_alive = server; // dropped at the end of the scope

    let mut client = open_client(&mut guard);

    // initialize: the bridge answers with our protocol version and identity.
    let result = client.request(
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "smoke-test", "version": "1"},
        }),
    );
    assert_eq!(result["protocolVersion"], "2025-11-25");
    assert_eq!(result["serverInfo"]["name"], "robrix-tools");
    assert_eq!(result["capabilities"]["tools"]["listChanged"], false);

    // The initialized notification is answered with silence.
    client.notify("notifications/initialized", json!({}));
    client.expect_silence(Duration::from_millis(500));

    // tools/list: the session's full tool set — the capability-gated reads,
    // the generator, and the two ungated native tools — no cursor.
    let result = client.request("tools/list", json!({}));
    assert!(result.get("nextCursor").is_none(), "never paginate");
    let tools = result["tools"].as_array().expect("tools is a list");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec![
            "read_room_messages",
            "read_older_messages",
            "room_info",
            "list_rooms",
            "read_other_room_messages",
            "list_spaces",
            "space_info",
            "list_space_rooms",
            "launch_splash_app",
            "read_room_memory",
            "send_message",
            "post_room_message",
        ]
    );
    for tool in tools {
        assert!(tool["description"].as_str().unwrap().len() > 20);
        assert_eq!(tool["inputSchema"]["type"], "object");
    }

    // launch_splash_app: the stub host records the description and its JSON
    // summary comes back as the model-visible text.
    let result = client.request(
        "tools/call",
        json!({"name": "launch_splash_app", "arguments": {"description": "a counter app"}}),
    );
    assert_eq!(result["isError"], false);
    let summary: Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(summary["app_id"], "smoke-app");
    assert_eq!(summary["status"], "installed_and_running");

    // send_message: plain text back.
    let result = client.request(
        "tools/call",
        json!({"name": "send_message", "arguments": {"text": "hello room"}}),
    );
    assert_eq!(result["isError"], false);
    assert_eq!(result["content"][0]["text"], "posted");

    // The host (this process) saw both calls with the model's arguments.
    let calls = host.calls();
    assert_eq!(
        calls,
        vec![
            "launch_splash_app(\"a counter app\")".to_string(),
            "send_message(\"hello room\")".to_string(),
        ]
    );

    // A blank description trips the tool's real validation: isError true, so
    // the reason reaches the model.
    let result = client.request(
        "tools/call",
        json!({"name": "launch_splash_app", "arguments": {"description": "   "}}),
    );
    assert_eq!(result["isError"], true);
    assert!(result["content"][0]["text"].as_str().unwrap().contains("must not be empty"));

    // An unknown tool is a JSON-RPC error (-32602), not a tool failure.
    let reply = client.raw_request(
        "tools/call",
        json!({"name": "no_such_tool", "arguments": {}}),
    );
    assert_eq!(reply["error"]["code"], -32602);
    assert!(reply["error"]["message"].as_str().unwrap().contains("no_such_tool"));

    // Agent ends the session by closing its stdin: the relay sees EOF on its
    // input pump and exits cleanly.
    drop(client);
    let code = wait_for_exit(&mut guard, EXIT_TIMEOUT);
    assert_eq!(code, Some(0), "the relay must exit 0 when the agent closes stdin");
}

/// The host side ending the session (dropping the [`ToolServer`]) must close
/// the live connection, so the relay child sees EOF and exits on its own —
/// that is how "Robrix ended the session" reaches the agent as a dead MCP
/// server without the agent having to do anything.
#[test]
fn the_relay_exits_when_the_host_session_ends() {
    let (server, mut guard, _host) = host_and_relay();
    let mut client = open_client(&mut guard);

    // A round trip first: proves the connection is alive AND registered (so
    // the teardown below is closing a registered connection, not racing it).
    let result = client.request(
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "smoke-test", "version": "1"},
        }),
    );
    assert_eq!(result["protocolVersion"], "2025-11-25");

    // The session ends: server dropped, socket closed. The relay must exit 0
    // without us touching its stdin.
    drop(server);
    drop(client);
    let code = wait_for_exit(&mut guard, EXIT_TIMEOUT);
    assert_eq!(code, Some(0), "the relay must exit 0 when the host ends the session");
}

/// Sanity: a process spawned with `--mcp-bridge` but no socket is refused up
/// front (usage error, exit 1) rather than falling through to the UI.
#[test]
fn a_bridge_spawn_missing_its_socket_fails_fast() {
    let status: ExitStatus = Command::new(env!("CARGO_BIN_EXE_robrix"))
        .arg("--mcp-bridge")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn the binary");
    assert_eq!(status.code(), Some(1));
}
