//! A minimal Agent Client Protocol (ACP) client over an [`AgentChannel`].
//!
//! Speaks newline-delimited JSON-RPC 2.0 to whichever transport a launcher
//! connected (by default a spawned `octos acp` child over its OS pipes, but
//! any ACP agent binary works — the protocol is the standard one from
//! <https://agentclientprotocol.com>). The pipe/process choice lives behind
//! [`AgentChannel`]; this client only sees frames. Deliberately
//! dependency-light: plain `std::thread` + `mpsc`, no async runtime. Incoming
//! events are queued for the UI thread, which drains them from `handle_event`;
//! each queued event is followed by a `SignalToUI` wakeup so replies that land
//! while the app is idle don't sit in the channel until the next input event.
//!
//! One client == one connection == one generation. The pipeline starts a
//! fresh agent per create-app request and drops it when the request completes,
//! so there is no reconnect/restart state to manage; a cancel is just a
//! `session/cancel` (and ultimately a channel close on drop).
//!
//! Threading: a writer thread and a reader thread touch the channel, and
//! neither may block the other or the UI. All outgoing frames go through the
//! dedicated writer thread fed by a bounded channel — the UI thread and the
//! reader thread only ever `try_send()`, so a stalled/wedged peer can never
//! freeze the UI (or deadlock the reader against its own refusal replies).
//! `Drop` closes the channel FIRST (which, for a subprocess, kills the child;
//! kill takes no locks), unblocking any thread stuck mid-write/mid-read.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};

use makepad_widgets::SignalToUI;
use serde_json::{json, Value};

use crate::channel::{AgentChannel, ChannelError, MAX_FRAME_BYTES};
use crate::launcher::{AgentLauncher, ExternalCommandLauncher, LaunchConfig};
use crate::mcp::McpServerConfig;
use crate::host_broker::HostBroker;

/// Cap on one incoming NDJSON line. A frame past this is not a protocol we
/// can parse anyway (real replies are a few KB) — treat it as a dead agent
/// rather than buffering without bound.
const MAX_LINE_BYTES: usize = MAX_FRAME_BYTES;
const MAX_QUEUED_BYTES: usize = 2 * MAX_LINE_BYTES;

/// Events surfaced to the UI thread, already reduced from raw JSON-RPC to what
/// the generation pipeline cares about.
#[derive(Debug)]
pub enum AcpEvent {
    /// `initialize` + `session/new` completed; the agent is ready for a prompt.
    SessionReady,
    /// A streamed chunk of the agent's reply text (`agent_message_chunk`).
    Chunk(String),
    /// The agent invoked a tool. `id` is its ACP `toolCallId` (how the
    /// matching completion is correlated); `title` is the tool name/title the
    /// agent reported. Shown as status, not blocked on.
    ToolCall { id: String, title: String },
    /// The agent finished a tool call. `id` matches the earlier
    /// [`AcpEvent::ToolCall`]; `ok` is false for a failed call; `summary` is
    /// the output preview the agent carried, when any. This is what closes
    /// the live card for a tool Robrix does not itself execute (octos's own
    /// `web_search` / `web_fetch` / `browser`).
    ToolCallDone { id: String, ok: bool, summary: String },
    /// A chunk of the agent's extended *thinking* (`agent_thought_chunk`).
    /// This is the only thing a reasoning model emits during the long quiet
    /// stretch before it starts writing, so dropping it (as this client used
    /// to) left the console frozen on one line for a minute at a time.
    Thought(String),
    /// The agent's plan (`plan`) — Claude Code turns its TodoWrite calls into
    /// this, so it's a real, ordered account of what it's about to do.
    Plan(Vec<PlanStep>),
    /// A session update that carries no content we show (a tool-call status
    /// change, a mode/command list). Surfaced anyway as proof of life: the
    /// stall watchdog measures the gap since the last event, and silently
    /// dropping these made a busy agent look hung.
    Tick,
    /// The prompt turn finished with this ACP stop reason (e.g. `end_turn`,
    /// `cancelled`, `refusal`), plus the full accumulated reply text.
    TurnDone { stop_reason: String, text: String },
    /// A JSON-RPC error reply, or a protocol-level failure.
    Error(String),
    /// The agent process exited (or its stdout closed). Carries a short
    /// diagnostic assembled from captured stderr — this is how "octos isn't
    /// installed / no provider configured" reaches the user.
    ProcessGone(String),
}

/// One step of the agent's plan.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanStep {
    pub content: String,
    /// `pending` | `in_progress` | `completed` (ACP's vocabulary).
    pub status: String,
}

fn event_size(event: &AcpEvent) -> usize {
    match event {
        AcpEvent::Chunk(text) | AcpEvent::Thought(text) | AcpEvent::Error(text) | AcpEvent::ProcessGone(text) => text.len(),
        AcpEvent::ToolCall { id, title } => id.len() + title.len(),
        AcpEvent::ToolCallDone { id, summary, .. } => id.len() + summary.len(),
        AcpEvent::Plan(steps) => steps.iter().map(|step| step.content.len() + step.status.len()).sum(),
        AcpEvent::TurnDone { stop_reason, text } => stop_reason.len() + text.len(),
        AcpEvent::SessionReady | AcpEvent::Tick => 0,
    }
}

/// Which JSON-RPC request an outstanding id belongs to. The pipeline runs
/// strictly one request at a time (initialize → session/new → prompt → ...),
/// so a single pending slot replaces a request table.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Pending {
    Initialize,
    NewSession,
    Prompt,
}

/// State shared between the UI-side handle and the reader thread. The reader
/// thread advances the handshake itself (initialize reply → send session/new)
/// so the UI only ever sees `SessionReady`.
struct Shared {
    /// Feed to the writer thread; admission never blocks. `None` after shutdown.
    write_tx: Mutex<Option<SyncSender<String>>>,
    queued_bytes: AtomicUsize,
    queued_event_bytes: AtomicUsize,
    turn_bytes: AtomicUsize,
    closed: AtomicBool,
    failure: Mutex<Option<String>>,
    broker: Option<Arc<HostBroker>>,
    next_id: AtomicU64,
    /// The id and kind of the single in-flight request, if any.
    pending: Mutex<Option<(u64, Pending)>>,
    /// The negotiated session id, once `session/new` returns.
    session_id: Mutex<Option<String>>,
    /// The agent's reply text accumulated from streamed chunks this turn.
    turn_text: Mutex<String>,
    /// Absolute workspace dir sent as the session's `cwd`.
    workspace: String,
    /// The stdio MCP servers advertised in `session/new` (`mcpServers`), so
    /// the agent's model can call Robrix's host tools. Usually empty — the
    /// create-app pipeline needs no tools — and set only by a session host
    /// that runs a tool server (see the app's `a2app::ai::session`).
    mcp_servers: Vec<McpServerConfig>,
}

impl Shared {
    fn write_line(&self, line: String) {
        if self.closed.load(Ordering::Acquire) { return; }
        let bytes = line.len();
        if bytes >= MAX_LINE_BYTES || self.queued_bytes.fetch_update(Ordering::AcqRel, Ordering::Acquire,
            |queued| queued.checked_add(bytes).filter(|total| *total <= MAX_QUEUED_BYTES)).is_err()
        {
            self.fail("Agent exceeded the protocol output limit.");
            return;
        }
        if let Some(tx) = self.write_tx.lock().unwrap().as_ref() {
            if tx.try_send(line).is_ok() { return; }
        }
        self.queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
        self.fail("Agent stopped reading its protocol requests.");
    }

    fn fail(&self, error: &str) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            makepad_widgets::log!("acp-client: FAIL: {error}");
            *self.failure.lock().unwrap() = Some(error.into());
            if let Some(broker) = &self.broker { broker.stop(); }
            SignalToUI::set_ui_signal();
        }
    }

    fn send_request(&self, kind: Pending, method: &str, params: Value) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        *self.pending.lock().unwrap() = Some((id, kind));
        makepad_widgets::log!("acp-client: -> {method} (id {id})");
        self.write_line(
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string(),
        );
    }
}

/// A live connection to an ACP agent over an [`AgentChannel`].
pub struct AcpClient {
    /// Kept so `Drop` can tear the transport down; also shared with the
    /// reader thread, which is why it is an `Arc` rather than a plain box.
    channel: Arc<dyn AgentChannel>,
    events: Receiver<(usize, AcpEvent)>,
    shared: Arc<Shared>,
    /// Human-readable command line, for diagnostics.
    cmd_desc: String,
}

impl AcpClient {
    /// Spawns `cmd_line` (whitespace-split; first token is the binary) with
    /// `workspace` as its working directory, and starts the ACP handshake.
    /// `env` is layered on top of the inherited environment — that's how the
    /// UI's model/effort/thinking picks reach the agent, since these are the
    /// agent's own env-var knobs rather than anything in the ACP protocol.
    /// Returns an error string suitable to show the user if the process cannot
    /// be spawned at all.
    pub fn spawn(
        cmd_line: &str,
        workspace: &std::path::Path,
        env: &[(String, String)],
        extra_args: &[String],
        mcp_servers: &[McpServerConfig],
    ) -> Result<Self, String> {
        let launcher = ExternalCommandLauncher::new(cmd_line.to_string(), env.to_vec(), extra_args.to_vec());
        let cfg = LaunchConfig { workspace, mcp_servers, broker: None };
        let launched = launcher.launch(&cfg)?;
        Ok(Self::new(launched.channel, &launched.desc, launched.broker, workspace, mcp_servers))
    }

    /// Builds the protocol machinery over an already-connected channel and
    /// starts the ACP handshake. Transport-agnostic: the same code drives a
    /// spawned child's pipes and an ExtensionFoundation XPC connection.
    pub(crate) fn new(
        channel: Box<dyn AgentChannel>,
        cmd_desc: &str,
        broker: Option<Arc<HostBroker>>,
        workspace: &std::path::Path,
        mcp_servers: &[McpServerConfig],
    ) -> Self {
        let channel: Arc<dyn AgentChannel> = Arc::from(channel);
        let (tx, events) = std::sync::mpsc::sync_channel::<(usize, AcpEvent)>(128);
        let (write_tx, write_rx) = std::sync::mpsc::sync_channel::<String>(16);
        let shared = Arc::new(Shared {
            write_tx: Mutex::new(Some(write_tx)),
            queued_bytes: AtomicUsize::new(0), closed: AtomicBool::new(false),
            queued_event_bytes: AtomicUsize::new(0), turn_bytes: AtomicUsize::new(0),
            failure: Mutex::new(None), broker,
            next_id: AtomicU64::new(1),
            pending: Mutex::new(None),
            session_id: Mutex::new(None),
            turn_text: Mutex::new(String::new()),
            workspace: workspace.to_string_lossy().into_owned(),
            mcp_servers: mcp_servers.to_vec(),
        });

        // Writer thread: sole writer of the channel. Exits when every Sender
        // is gone (client dropped) or the transport breaks (peer died).
        let writer_shared = Arc::downgrade(&shared);
        {
            let channel = channel.clone();
            std::thread::spawn(move || {
                for line in write_rx {
                    let result = channel.send_frame(line.as_bytes());
                    if let Some(shared) = writer_shared.upgrade() {
                        shared.queued_bytes.fetch_sub(line.len(), Ordering::AcqRel);
                        if result.is_err() { shared.fail("Agent protocol input closed."); }
                    }
                    if result.is_err() {
                        break;
                    }
                }
            });
        }

        // Reader thread: frames → reduced AcpEvents → queue + UI signal. The
        // channel enforces its own per-frame cap, so a malformed agent can't
        // wedge or balloon the host app.
        {
            let tx = tx.clone();
            let shared = shared.clone();
            let channel = channel.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                loop {
                    match channel.recv_frame(&mut buf) {
                        Ok(()) => {}
                        Err(ChannelError::Oversized) => {
                            shared.fail("Agent sent an oversized protocol frame.");
                            return;
                        }
                        Err(_) => break,
                    }
                    let line = String::from_utf8_lossy(&buf);
                    if line.trim().is_empty() {
                        continue;
                    }
                    makepad_widgets::log!("acp-client: <- {}", line.chars().take(200).collect::<String>());
                    for event in reduce_line(&shared, &line) {
                        let size = event_size(&event);
                        if shared.queued_event_bytes.fetch_update(Ordering::AcqRel, Ordering::Acquire,
                            |queued| queued.checked_add(size).filter(|total| *total <= MAX_QUEUED_BYTES)).is_err()
                        {
                            shared.fail("Agent exceeded the pending event byte limit.");
                            return;
                        }
                        if tx.try_send((size, event)).is_err() {
                            shared.queued_event_bytes.fetch_sub(size, Ordering::AcqRel);
                            shared.fail("Agent exceeded the pending event limit.");
                            return; // client dropped; stop reading
                        }
                        SignalToUI::set_ui_signal();
                    }
                    if shared.closed.load(Ordering::Acquire) { return; }
                }
                // EOF revokes broker work immediately. Diagnostic draining
                // must not extend a disconnected peer's active turn.
                if let Some(broker) = &shared.broker { broker.stop(); }
                // The channel closed. Wait briefly for its diagnostics drain to
                // finish flushing the reason (bounded, so an open stderr that
                // never closes can't hang teardown).
                channel.wait_diagnostics(std::time::Duration::from_millis(1000));
                let tail = if shared.broker.is_some() { String::new() } else { channel.diagnostics() };
                let msg = if tail.trim().is_empty() {
                    "agent process exited".to_string()
                } else {
                    format!("agent process exited: {}", tail.trim())
                };
                if tx.try_send((0, AcpEvent::ProcessGone(msg.clone()))).is_err() {
                    // Failure is stored outside the bounded event queue, so
                    // EOF cannot disappear behind a full queue of updates.
                    shared.fail(&msg);
                }
                SignalToUI::set_ui_signal();
            });
        }

        // Kick off the handshake; the reader thread chains session/new when
        // the initialize reply arrives.
        shared.send_request(
            Pending::Initialize,
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": shared.broker.as_ref().map_or(json!({}), |broker| json!({"_meta": {
                    octos_llm::host::CAPABILITY_KEY: broker.config(),
                }})),
                "clientInfo": {"name": "robrix", "version": env!("CARGO_PKG_VERSION")},
            }),
        );

        Self { channel, events, shared, cmd_desc: cmd_desc.to_string() }
    }

    /// Drains every event queued by the reader thread. Call from the UI
    /// thread's event handler (a `SignalToUI` wakeup guarantees one fires).
    pub fn drain_events(&mut self) -> Vec<AcpEvent> {
        let failure = self.shared.failure.lock().unwrap().take();
        if let Some(error) = failure {
            self.channel.close();
            return vec![AcpEvent::ProcessGone(error)];
        }
        let mut out = Vec::new();
        while let Ok((size, e)) = self.events.try_recv() {
            self.shared.queued_event_bytes.fetch_sub(size, Ordering::AcqRel);
            out.push(e);
        }
        out
    }

    /// Sends the user's prompt for this session. `SessionReady` must have been
    /// received first. Repair prompts reuse the same session, so the agent
    /// keeps the conversation history across attempts.
    pub fn send_prompt(&mut self, text: &str) {
        let Some(session_id) = self.shared.session_id.lock().unwrap().clone() else {
            return;
        };
        if let Some(broker) = &self.shared.broker {
            if let Err(error) = broker.begin_turn() { self.shared.fail(&error); return; }
        }
        self.shared.turn_text.lock().unwrap().clear();
        self.shared.turn_bytes.store(0, Ordering::Release);
        self.shared.send_request(
            Pending::Prompt,
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": text}],
            }),
        );
    }

    /// Requests cancellation of the in-flight turn (`session/cancel`). The
    /// turn still ends with a `TurnDone{stop_reason:"cancelled"}` reply.
    pub fn cancel(&mut self) {
        if let Some(broker) = &self.shared.broker { broker.cancel(); }
        let Some(session_id) = self.shared.session_id.lock().unwrap().clone() else {
            return;
        };
        self.shared.write_line(
            json!({
                "jsonrpc": "2.0",
                "method": "session/cancel",
                "params": {"sessionId": session_id},
            })
            .to_string(),
        );
    }

    /// The command line this client was spawned with (for error messages).
    pub fn cmd_desc(&self) -> &str {
        &self.cmd_desc
    }
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        if let Some(broker) = &self.shared.broker { broker.stop(); }
        // Close FIRST: for a subprocess channel this kills the child, which
        // takes no locks and closes the pipes, so any thread blocked on the
        // peer (reader mid-read, writer mid-write) unwedges.
        self.channel.close();
        // Dropping the sender lets the writer thread exit.
        self.shared.write_tx.lock().unwrap().take();
    }
}

/// The `session/new` params a session starts with: the workspace dir the
/// agent works in, plus every stdio MCP server the agent may connect to.
/// `mcpServers` is where a session's tool server reaches the model — Robrix
/// registers itself (the `--mcp-bridge` relay child), so a model that decides
/// to call a host tool spawns a relay whose stdio lands back in Robrix.
fn session_new_params(workspace: &str, mcp_servers: &[McpServerConfig]) -> Value {
    json!({
        "cwd": workspace,
        "mcpServers": mcp_servers.iter().map(|server| json!({
            "name": server.name,
            "command": server.command,
            "args": server.args,
            // The ACP schema REQUIRES `env` on a stdio server; without it the
            // agent's own deserializer (VecSkipError) silently drops the
            // server before its handler ever sees it. Robrix forwards no
            // extra environment: the relay child inherits the agent's own
            // (sanitized) environment.
            "env": [],
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod params_tests {
    use super::*;

    #[test]
    fn session_new_advertises_no_servers_by_default() {
        assert_eq!(
            session_new_params("/tmp/work", &[]),
            json!({"cwd": "/tmp/work", "mcpServers": []})
        );
    }

    #[test]
    fn session_new_carries_a_tool_servers_command_and_args() {
        let servers = [McpServerConfig::new(
            "robrix-tools",
            "/usr/bin/robrix",
            vec!["--mcp-bridge".to_string(), "--socket".to_string(), "/tmp/s/tools.sock".to_string()],
        )];
        let value = session_new_params("/tmp/work", &servers);
        assert_eq!(value["cwd"], "/tmp/work");
        let advertised = value["mcpServers"].as_array().unwrap();
        assert_eq!(advertised.len(), 1);
        assert_eq!(advertised[0]["name"], "robrix-tools");
        assert_eq!(advertised[0]["command"], "/usr/bin/robrix");
        assert_eq!(
            advertised[0]["args"],
            json!(["--mcp-bridge", "--socket", "/tmp/s/tools.sock"])
        );
        assert_eq!(
            advertised[0]["env"],
            json!([]),
            "the ACP schema requires `env` on a stdio server; without it the \
             server is silently dropped by the agent's VecSkipError deserializer"
        );
    }

    /// The whole point: the config a session host passes to `spawn` must
    /// actually ride the wire to the agent in `session/new`. Here a canned
    /// stdio agent records the request it receives; the app's own session
    /// tests exercise a real agent shape end to end.
    #[cfg(unix)]
    #[test]
    fn spawned_agent_sees_the_tool_servers_in_session_new() {
        use std::os::unix::fs::PermissionsExt;
        // Fresh run each time: the recording file outlives the test (this dir
        // is shared, not a per-run tempdir), and a stale capture would silently
        // pass assertions against an old wire format.
        let dir = std::env::temp_dir().join("acp_mcpservers_wire_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("record.sh");
        let out = dir.join("session_new.json");
        let _ = std::fs::remove_file(&out);
        // Reads stdin; answers `initialize` (its id is always 1, the first
        // request this client sends) so the reader thread proceeds to
        // session/new, and copies that request to `$out` before replying.
        std::fs::write(
            &script,
            format!(
                r#"#!/bin/sh
out="{out}"
while IFS= read -r line; do
  case "$line" in
    *initialize*) echo '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1,"agentCapabilities":{{}}}}}}' ;;
    *session/new*) echo "$line" > "$out" ;;
  esac
done
"#,
                out = out.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let servers = [McpServerConfig::new(
            "robrix-tools",
            "/usr/bin/robrix",
            vec!["--mcp-bridge".to_string(), "--socket".to_string(), "/tmp/s/tools.sock".to_string()],
        )];
        let mut client =
            AcpClient::spawn(script.to_str().unwrap(), &dir, &[], &[], &servers).unwrap();
        // Let the reader thread drive the handshake to session/new.
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            if out.exists() {
                break;
            }
            let _ = client.drain_events();
        }
        drop(client);

        let line = std::fs::read_to_string(&out).expect("the agent recorded session/new");
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["params"]["cwd"], dir.to_string_lossy().into_owned());
        let advertised = value["params"]["mcpServers"].as_array().unwrap();
        assert_eq!(advertised.len(), 1);
        assert_eq!(advertised[0]["name"], "robrix-tools");
        assert_eq!(advertised[0]["command"], "/usr/bin/robrix");
        assert_eq!(advertised[0]["args"][0], "--mcp-bridge");
        assert_eq!(advertised[0]["args"][2], "/tmp/s/tools.sock");
        // The env field must ride the wire too (schema-required) or the server
        // is dropped before the agent's handler sees it.
        assert_eq!(advertised[0]["env"], json!([]));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn protected_state(session_id: Option<&str>, pending: Option<(u64, Pending)>) -> Arc<Shared> {
        Arc::new(Shared {
            write_tx: Mutex::new(None), queued_bytes: AtomicUsize::new(0),
            queued_event_bytes: AtomicUsize::new(0), turn_bytes: AtomicUsize::new(0),
            closed: AtomicBool::new(false), failure: Mutex::new(None),
            broker: Some(HostBroker::for_protocol_test()), next_id: AtomicU64::new(1),
            pending: Mutex::new(pending), session_id: Mutex::new(session_id.map(str::to_string)),
            turn_text: Mutex::new(String::new()), workspace: "/".into(), mcp_servers: Vec::new(),
        })
    }

    #[test]
    fn protected_protocol_rejects_malformed_frames_and_unknown_sessions() {
        for line in [
            "{broken",
            r#"{"jsonrpc":"1.0","method":"session/update","params":{"sessionId":"room"}}"#,
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"other"}}"#,
            r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#,
        ] {
            let shared = protected_state(Some("room"), None);
            assert!(reduce_line(&shared, line).is_empty());
            assert!(shared.closed.load(Ordering::Acquire));
        }
        let before_session = protected_state(None, None);
        assert!(reduce_line(&before_session, r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#).is_empty());
        assert!(before_session.closed.load(Ordering::Acquire), "two absent session IDs are not a valid identity match");
    }

    #[test]
    fn protected_session_handshake_rejects_missing_empty_or_oversized_ids() {
        for result in [json!({}), json!({"sessionId":""}), json!({"sessionId":"x".repeat(129)})] {
            let shared = protected_state(None, Some((1, Pending::NewSession)));
            let reply = json!({"jsonrpc":"2.0","id":1,"result":result});
            assert!(reduce_line(&shared, &reply.to_string()).is_empty());
            assert!(shared.closed.load(Ordering::Acquire));
            assert!(shared.session_id.lock().unwrap().is_none());
        }
    }

    /// Regression test for the review-confirmed deadlock: an agent that
    /// floods client-bound requests WITHOUT reading its stdin used to wedge
    /// the reader thread inside a blocking refusal write (holding the stdin
    /// lock), which froze cancel/Drop — and the UI thread with them. With the
    /// writer thread + kill-first Drop, teardown must stay prompt no matter
    /// what the child does.
    #[test]
    fn burst_agent_cannot_wedge_teardown() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join("hl_acp_burst_test");
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("burst.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\n\
             i=0\n\
             while [ $i -lt 3000 ]; do\n\
               echo '{\"jsonrpc\":\"2.0\",\"id\":\"r'$i'\",\"method\":\"nag\",\"params\":{}}'\n\
               i=$((i+1))\n\
             done\n\
             sleep 30\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut client = AcpClient::spawn(script.to_str().unwrap(), &dir, &[], &[], &[]).unwrap();
        // Let the flood arrive (the client refuses each request via the
        // writer channel; the child never drains stdin, so the pipe fills).
        std::thread::sleep(std::time::Duration::from_millis(600));
        let _ = client.drain_events();
        let start = std::time::Instant::now();
        client.cancel(); // no session yet — must be a prompt no-op
        drop(client); // kill-first: must unwedge everything
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "teardown wedged on a non-draining agent"
        );
    }
}

/// Pulls the short output preview out of an ACP `tool_call_update`'s
/// `content` array, regardless of which `ToolCallContent` variant wrapped it.
/// Empty when the update carries none (the common case for a tool that just
/// reports a status).
fn tool_call_output_preview(update: &Value) -> String {
    update
        .get("content")
        .and_then(Value::as_array)
        .and_then(|entries| {
            entries.iter().find_map(|entry| {
                entry
                    .pointer("/content/text")
                    .and_then(Value::as_str)
                    .or_else(|| entry.get("text").and_then(Value::as_str))
            })
        })
        .unwrap_or_default()
        .to_string()
}

/// Reduces one incoming JSON-RPC line to zero or more `AcpEvent`s, advancing
/// the handshake as a side effect. Runs on the reader thread; only touches
/// `Shared`, never the UI.
fn reduce_line(shared: &Arc<Shared>, line: &str) -> Vec<AcpEvent> {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        if shared.broker.is_some() { shared.fail("The protected agent sent invalid JSON."); }
        return vec![];
    };
    if shared.broker.is_some() && value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        shared.fail("The protected agent sent an invalid JSON-RPC version.");
        return vec![];
    }

    // Notifications: session/update carries the streamed turn content.
    if value.get("method").and_then(Value::as_str) == Some("session/update") {
        if shared.broker.is_some() {
            let session_id = shared.session_id.lock().unwrap();
            if session_id.is_none() || value.pointer("/params/sessionId").and_then(Value::as_str) != session_id.as_deref() {
                shared.fail("Agent update used an unknown session.");
                return vec![];
            }
        }
        if shared.turn_bytes.fetch_add(line.len(), Ordering::AcqRel).saturating_add(line.len()) > MAX_LINE_BYTES {
            shared.fail("Agent exceeded the turn output limit.");
            return vec![];
        }
        let Some(update) = value.pointer("/params/update") else {
            return vec![];
        };
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("agent_message_chunk") => {
                let text = update
                    .pointer("/content/text")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if text.is_empty() {
                    return vec![];
                }
                shared.turn_text.lock().unwrap().push_str(text);
                return vec![AcpEvent::Chunk(text.to_string())];
            }
            Some("agent_thought_chunk") => {
                let text = update
                    .pointer("/content/text")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if text.is_empty() {
                    return vec![];
                }
                // NOT appended to `turn_text`: thinking is not part of the
                // reply, and folding it in would feed the fence extractor
                // whatever code the model mused about mid-thought.
                return vec![AcpEvent::Thought(text.to_string())];
            }
            Some("tool_call") => {
                let id = update
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let title = update
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                return vec![AcpEvent::ToolCall { id, title }];
            }
            Some("tool_call_update") => {
                // The terminal status of a call the agent started. A
                // pending/in-progress update is just proof of life.
                let status = update.get("status").and_then(Value::as_str).unwrap_or("");
                if status == "completed" || status == "failed" {
                    let id = update
                        .get("toolCallId")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    return vec![AcpEvent::ToolCallDone {
                        id,
                        ok: status == "completed",
                        summary: tool_call_output_preview(update),
                    }];
                }
                return vec![AcpEvent::Tick];
            }
            Some("plan") => {
                let Some(entries) = update.get("entries").and_then(Value::as_array) else {
                    return vec![AcpEvent::Tick];
                };
                let steps: Vec<PlanStep> = entries
                    .iter()
                    .filter_map(|e| {
                        let content = e.get("content").and_then(Value::as_str)?.trim();
                        (!content.is_empty()).then(|| PlanStep {
                            content: content.to_string(),
                            status: e
                                .get("status")
                                .and_then(Value::as_str)
                                .unwrap_or("pending")
                                .to_string(),
                        })
                    })
                    .collect();
                if steps.is_empty() {
                    return vec![AcpEvent::Tick];
                }
                return vec![AcpEvent::Plan(steps)];
            }
            // Everything else (tool_call_update, current_mode_update,
            // available_commands_update, user_message_chunk…): no content to
            // show, but still proof the agent is alive.
            _ => return vec![AcpEvent::Tick],
        }
    }

    // Protected requests use the host broker. Ordinary ACP can request
    // permissions or client filesystem access; this client has no approval
    // UI or filesystem service, so refuse explicitly instead of leaving the
    // agent waiting. JSON-RPC ids may be numbers OR strings; echo either.
    if value.get("method").is_some() {
        if let Some(id) = value.get("id").filter(|id| !id.is_null()) {
            let method = value.get("method").and_then(Value::as_str).unwrap_or("?");
            if let Some(broker) = &shared.broker {
                let responder = shared.clone();
                let response_id = id.clone();
                let reply = Box::new(move |result: Result<Value, String>| {
                    let response = match result {
                        Ok(result) => json!({"jsonrpc":"2.0", "id":response_id, "result":result}),
                        Err(error) => json!({"jsonrpc":"2.0", "id":response_id, "error":{"code":-32000,"message":error}}),
                    };
                    responder.write_line(response.to_string());
                });
                if let Err(error) = broker.submit(id, method, value.get("params").cloned().unwrap_or(json!({})), line.len(), reply) {
                    shared.write_line(json!({"jsonrpc":"2.0", "id":id, "error":{"code":-32000,"message":error}}).to_string());
                }
                return vec![];
            }
            shared.write_line(
                json!({
                    "jsonrpc": "2.0",
                    "id": id.clone(),
                    "error": {"code": -32601, "message": format!("client does not support {method}")},
                })
                .to_string(),
            );
        }
        // Method + no id = an unrecognized notification; nothing to do.
        return vec![];
    }

    // Replies: routed by the single outstanding request id (ours are numeric).
    let Some(id) = value.get("id").and_then(Value::as_u64) else {
        return vec![];
    };
    let pending = {
        let mut p = shared.pending.lock().unwrap();
        match *p {
            Some((pid, kind)) if pid == id => {
                *p = None;
                Some(kind)
            }
            _ => None,
        }
    };
    let Some(pending) = pending else { return vec![] };

    if let Some(err) = value.get("error") {
        makepad_widgets::log!("acp-client: <- error for {pending:?}: {err}");
        if let Some(broker) = &shared.broker {
            if pending == Pending::Prompt { broker.cancel(); }
            else { shared.fail("The protected agent handshake failed."); return vec![]; }
        }
        // Forward the WHOLE error object, not just `message`. JSON-RPC's
        // `message` is the transport's own generic string — octos sends
        // "Internal error" — while the provider's actual sentence ("You've
        // reached your usage limit…") lives in `data`. Taking `message` alone
        // discarded it here, before anything downstream could read it, which is
        // why the UI could only ever say "internal error".
        // `pipeline::short_reason` digs the specific message back out.
        let msg = if err.get("data").is_some() {
            err.to_string()
        } else {
            err.get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown agent error")
                .to_string()
        };
        return vec![AcpEvent::Error(msg)];
    }

    match pending {
        Pending::Initialize => {
            makepad_widgets::log!("acp-client: <- initialize reply");
            if let Some(broker) = &shared.broker {
                if let Err(error) = broker.accept_handshake(&value["result"]) {
                    shared.fail(&error);
                    return vec![];
                }
            }
            // Handshake step 2, driven from here so the UI needn't care.
            shared.send_request(
                Pending::NewSession,
                "session/new",
                session_new_params(&shared.workspace, &shared.mcp_servers),
            );
            vec![]
        }
        Pending::NewSession => {
            makepad_widgets::log!("acp-client: <- session/new reply: {}", value);
            let Some(sid) = value
                .pointer("/result/sessionId")
                .and_then(Value::as_str)
                .filter(|sid| shared.broker.is_none() || (!sid.is_empty() && sid.len() <= 128))
            else {
                if shared.broker.is_some() {
                    shared.fail("The protected agent returned an invalid session ID.");
                    return vec![];
                }
                return vec![AcpEvent::Error("session/new reply had no sessionId".into())];
            };
            *shared.session_id.lock().unwrap() = Some(sid.to_string());
            vec![AcpEvent::SessionReady]
        }
        Pending::Prompt => {
            makepad_widgets::log!("acp-client: <- prompt reply");
            if let Some(broker) = &shared.broker { broker.cancel(); }
            let stop_reason = value
                .pointer("/result/stopReason")
                .and_then(Value::as_str)
                .unwrap_or("end_turn")
                .to_string();
            let text = std::mem::take(&mut *shared.turn_text.lock().unwrap());
            vec![AcpEvent::TurnDone { stop_reason, text }]
        }
    }
}
