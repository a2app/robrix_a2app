//! Regression harness for the AI-room MCP tool bridge: does the *real*
//! `robrix --mcp-bridge` relay survive a session driven by the SAME rmcp-based
//! MCP client octos uses?
//!
//! Why this exists: [`tests/mcp_transport.rs`] proves the relay + socket with a
//! hand-written client, but that client cannot reproduce client-stack
//! behaviour — and the relay once died under rmcp right after `initialize`
//! ("MCP tools/list failed … Transport closed", relay "exited gracefully"
//! exit 0), so AI rooms silently lost their host tools and fell back to a
//! tool-less session. Root cause was NOT the relay: on macOS/BSD `accept()`
//! inherits the listener's `O_NONBLOCK`, and the tool server's blocking
//! serve loop read EAGAIN between requests and treated it as a dead peer,
//! dropping the connection (Linux does not inherit the flag, which is why it
//! only ever broke on macOS). The fix lives in
//! `robrix::a2app::ai::server` (`set_nonblocking(false)` on accepted
//! connections). This test drives the exact octos connection sequence end to
//! end and must keep both Robrix tools listable and callable.
//!
//! NOTE: like `mcp_transport.rs`, this binds a Unix socket, so it needs an
//! environment that permits that (a normal terminal, CI).

#![cfg(all(feature = "a2app", unix))]

use std::process::Stdio;
use std::sync::Mutex;
use std::sync::Arc;
use std::time::Duration;

use a2app_agent::mcp::McpServer;
use robrix::a2app::ai::server::ToolServer;
use robrix::a2app::ai::tools::{AiHost, register_session_tools};
use rmcp::model::{CallToolRequestParams, ClientInfo, Implementation};
use rmcp::service::{RoleClient, RunningService, serve_client};
use rmcp::transport::child_process::TokioChildProcess;
use serde_json::json;
use tokio::time::timeout;

/// Mirrors octos's own HANDSHAKE_TIMEOUT (octos-agent/src/mcp.rs).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Generous slack for a request/reply over the relay under a debug build.
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);

/// A stand-in for the real Robrix-side [`AiHost`], recording calls like the
/// one in `mcp_transport.rs`.
#[derive(Default)]
struct RecordingHost {
    calls: Mutex<Vec<String>>,
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
}

/// Binds an in-process session tool server and connects to it the way octos
/// does: spawn the `robrix --mcp-bridge` relay as an rmcp stdio child and run
/// the MCP initialize handshake. Returns the live service plus the host, so a
/// test can list tools, call one, and assert what the host saw.
async fn connect_via_rmcp_like_octos(
) -> (ToolServer, Arc<RunningService<RoleClient, ClientInfo>>, Arc<RecordingHost>) {
    let host: Arc<RecordingHost> = Arc::new(RecordingHost::default());
    let mut template = McpServer::new();
    register_session_tools(&mut template, host.clone());
    let server = ToolServer::bind(template).expect("bind the session socket");
    server.start().expect("start the accept loop");

    // The exact spawn octos does in connect_stdio (minus env sanitization,
    // which strips nothing the relay needs): kill_on_drop, inherited stderr so
    // the relay's own diagnostics surface on the test console.
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_robrix"));
    cmd.arg("--mcp-bridge").arg("--socket").arg(server.socket_path());
    cmd.kill_on_drop(true);
    let (transport, _stderr) = TokioChildProcess::builder(cmd)
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn `robrix --mcp-bridge`");

    let mut info = ClientInfo::default();
    info.client_info = Implementation::new("robrix-mcp-rmcp-repro", "0.1");
    let service = timeout(HANDSHAKE_TIMEOUT, serve_client(info, transport))
        .await
        .expect("MCP initialize handshake timed out")
        .expect("MCP initialize failed");
    (server, Arc::new(service), host)
}

/// The octos connection sequence end to end: initialize → tools/list must
/// return both Robrix tools, and a `send_message` call must reach the host.
/// If the relay dies after initialize (the AI-room symptom), this fails at
/// tools/list with the transport closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_rmcp_client_lists_and_calls_robrix_tools_over_the_relay() {
    let (_server, service, host) = connect_via_rmcp_like_octos().await;

    // tools/list: the relay must survive long enough to answer it. This is
    // where the AI-room runs died ("input stream terminated", relay exit 0).
    let tools = timeout(REPLY_TIMEOUT, service.list_all_tools())
        .await
        .expect("tools/list timed out — the relay likely died after initialize")
        .expect("tools/list failed");
    let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    assert!(names.iter().any(|n| n == "send_message"), "tools advertised: {names:?}");
    assert!(names.iter().any(|n| n == "launch_splash_app"), "tools advertised: {names:?}");

    // send_message round trip through rmcp -> relay -> socket -> host.
    let mut params = CallToolRequestParams::new("send_message");
    params.arguments = Some(json!({"text": "ping from rmcp"}).as_object().unwrap().clone());
    let result = timeout(REPLY_TIMEOUT, service.call_tool(params))
        .await
        .expect("tools/call timed out")
        .expect("tools/call failed");
    assert!(!result.is_error.unwrap_or(false), "send_message must not report isError");
    let host_calls = host.calls.lock().unwrap();
    assert!(
        host_calls.iter().any(|c| c.contains("ping from rmcp")),
        "the host must have seen the call: {host_calls:?}"
    );
}
