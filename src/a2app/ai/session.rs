//! The Robrix-side agent session that makes host tools reachable from a real
//! agent — the missing middle between the tool server ([`super::server`]) and
//! the tools ([`super::tools`]) on one side, and a spawned ACP agent on the
//! other.
//!
//! One [`AiSession`] belongs to one room and lives for as long as that room's
//! conversation does. It owns three things:
//!
//! 1. the session-scoped [`ToolServer`] (the socket the tools answer on),
//! 2. the tools, backed by a real [`AiHost`] ([`SessionHost`]) that marshals
//!    tool calls from the server's serve threads onto the UI thread and back,
//! 3. the long-lived ACP agent itself ([`a2app_agent::AgentTransport`]),
//!    spawned with a [`McpServerConfig`] that tells it to connect to (1) —
//!    when its model decides to call a tool it spawns `robrix --mcp-bridge`
//!    and its calls land here.
//!
//! The UI thread drives the session from the a2app runtime's event pass:
//! [`AiSession::advance`] drains agent events (whose `Reply`/`Error`/`Gone`
//! the runtime acts on), and the runtime drains the session's tool jobs
//! ([`SessionJob`]) each pass, executes the real work (start the generation
//! pipeline, post to the room), and answers the waiting tool call. Tool
//! execution therefore never blocks the UI: only the serve thread that ran
//! the tool waits, parked on a channel until the UI thread has done the work.
//!
//! Deliberately free of makepad and of sockets in its pure parts: the host
//! rendezvous below is plain `std::sync::mpsc` and unit-testable without a
//! Unix socket, which is what keeps the sandboxed test run meaningful here
//! (the socket halves are covered by `tests/mcp_transport.rs` on CI).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::Arc;

use a2app_agent::prefs::AgentPrefs;
use a2app_agent::acp_client::AcpEvent;
use a2app_agent::AgentTransport;
use a2app_agent::mcp::SERVER_NAME;
use a2app_agent::mcp::{McpServer, McpServerConfig};
use matrix_sdk::ruma::OwnedRoomId;

use super::server::ToolServer;
use super::tools::{AiHost, register_session_tools};

/// A tool call that arrived on a session's MCP serve thread, waiting for the
/// UI thread to execute it. Each variant carries the channel the answer goes
/// back on; the runtime drains jobs every event pass and sends the result,
/// which unblocks the serve thread (and, through the bridge, the agent).
pub enum SessionJob {
    /// `launch_splash_app`: run the app-generation pipeline against this
    /// room. The runtime answers on `answer` when the pipeline finishes — or
    /// refuses up front (no provider, another generation running).
    LaunchSplashApp { description: String, answer: Sender<Result<String, String>> },
    /// `send_message`: post plain text into the session's room.
    SendRoomMessage { text: String, answer: Sender<Result<String, String>> },
}

/// The real [`AiHost`]: hands each tool call to the UI thread and blocks on
/// the answer.
///
/// Tools hold this as `Arc<dyn AiHost>` on whichever MCP serve thread runs
/// the call. The channel is unbounded, so the `send` never blocks; the
/// blocking `recv` parks only the one serve thread, and the UI thread is free
/// to keep servicing events while the tool's work (a generation can take
/// minutes) is in flight.
pub struct SessionHost {
    jobs: Sender<SessionJob>,
}

impl AiHost for SessionHost {
    fn launch_splash_app(&self, description: &str) -> Result<String, String> {
        let (answer_tx, answer_rx) = channel();
        self.jobs
            .send(SessionJob::LaunchSplashApp {
                description: description.to_string(),
                answer: answer_tx,
            })
            .map_err(|_| "this session's UI thread is gone".to_string())?;
        answer_rx
            .recv()
            .map_err(|_| "this session ended before the app was built".to_string())?
    }

    fn send_room_message(&self, text: &str) -> Result<String, String> {
        let (answer_tx, answer_rx) = channel();
        self.jobs
            .send(SessionJob::SendRoomMessage {
                text: text.to_string(),
                answer: answer_tx,
            })
            .map_err(|_| "this session's UI thread is gone".to_string())?;
        answer_rx
            .recv()
            .map_err(|_| "this session ended before the message was posted".to_string())?
    }
}

/// What one [`AiSession::advance`] produced for the runtime to act on.
#[derive(Debug)]
pub enum SessionUpdate {
    /// The agent finished its handshake; queued prompts have started flowing.
    Ready,
    /// A turn completed with the agent's final text (empty replies are
    /// filtered out — a refusal or cancel posts nothing).
    Reply { text: String },
    /// The agent reported an error on the in-flight turn.
    Error(String),
    /// The agent process is gone (died, was killed, or never started). The
    /// session cannot be used again; the runtime drops it.
    Gone(String),
}

/// How [`AiSession::prompt`] disposed of a user request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptOutcome {
    /// Sent straight to the agent.
    Sent,
    /// The agent is mid-turn (or still starting up); the request is queued
    /// and will go out when the current turn ends.
    Queued,
    /// The agent is gone; the request was refused.
    Dead,
}

/// How many un-sent user requests a session will hold before dropping the
/// oldest. A chat that outruns a slow agent should degrade by forgetting the
/// oldest ask, not by buffering without bound.
const MAX_QUEUED_PROMPTS: usize = 16;

/// The next unique id for per-session agent workspaces (socket-like paths
/// stay unique even though a room may host several sessions over time).
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// One room's live AI agent session: the agent process, its tool server, and
/// the marshaling host connecting them.
///
/// Owned by the UI thread (the a2app runtime). Dropping it kills the agent
/// child (via [`AgentTransport`]'s own drop) and tears down the socket
/// ([`ToolServer`]'s drop closes live connections), so a relay child the
/// agent spawned sees EOF and exits.
pub struct AiSession {
    room_id: OwnedRoomId,
    transport: Box<dyn AgentTransport>,
    /// Kept for its drop: closes the socket and live connections. The tools
    /// on it answer through `jobs` below, so nothing here needs reading.
    _server: ToolServer,
    /// Tool calls from the server's serve threads, drained by the runtime.
    jobs: Receiver<SessionJob>,
    /// True once the agent reported `SessionReady`.
    ready: bool,
    /// True while a prompt is out (a turn is in flight); new prompts queue.
    busy: bool,
    /// User requests waiting for the agent to be free, oldest first.
    queued: VecDeque<String>,
    /// The tool call waiting on the generation the runtime is running for
    /// this session (if any). Answered when that generation completes, or
    /// with an error when the session ends first.
    generation_answer: Option<Sender<Result<String, String>>>,
    /// Set when the agent process is gone (`ProcessGone`); prompts are then
    /// refused and the runtime drops the session.
    transport_dead: bool,
}

impl AiSession {
    /// Binds this room's tool server and spawns the agent pointed at it.
    ///
    /// The agent is told (through `session/new` `mcpServers`) to treat the
    /// Robrix binary — this very process — as an MCP server whose socket is
    /// the freshly bound one. `Err` names why a session can't start (no
    /// provider, agent missing, socket bind failure).
    pub fn start(room_id: OwnedRoomId, prefs: AgentPrefs) -> Result<Self, String> {
        // The rendezvous: serve threads send jobs here, the UI thread drains.
        let (jobs_tx, jobs_rx) = channel::<SessionJob>();
        let host: Arc<dyn AiHost> = Arc::new(SessionHost { jobs: jobs_tx });

        let mut template = McpServer::new();
        register_session_tools(&mut template, host);
        let server = ToolServer::bind(template)?;
        server.start()?;

        // A dedicated workspace per session: the agent's tools are rooted at
        // its cwd, and a chat session lives far longer than a generation, so
        // it must not share the pipeline's scratch dir (files a stale run
        // left there would leak into unrelated work).
        let workspace = a2app_core::data_root()
            .join("ai_sessions")
            .join(format!("session-{}", NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&workspace)
            .map_err(|e| format!("couldn't create {}: {e}", workspace.display()))?;

        let exe = std::env::current_exe()
            .map_err(|e| format!("couldn't find this process's binary: {e}"))?;
        let tool_server = McpServerConfig::new(
            SERVER_NAME,
            exe.to_string_lossy().into_owned(),
            vec![
                "--mcp-bridge".to_string(),
                "--socket".to_string(),
                server.socket_path().to_string_lossy().into_owned(),
            ],
        );

        let transport =
            a2app_agent::start_backend_with_mcp(&workspace, &prefs, &[tool_server])?;

        Ok(Self {
            room_id,
            transport,
            _server: server,
            jobs: jobs_rx,
            ready: false,
            busy: false,
            queued: VecDeque::new(),
            generation_answer: None,
            transport_dead: false,
        })
    }

    /// The room this session chats in.
    pub fn room_id(&self) -> &OwnedRoomId {
        &self.room_id
    }

    /// Whether the agent finished its handshake and can take a prompt.
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Whether a turn is currently in flight.
    pub fn is_busy(&self) -> bool {
        self.busy
    }

    /// The tool calls waiting on the UI thread, drained by the runtime.
    pub fn drain_jobs(&mut self) -> Vec<SessionJob> {
        let mut jobs = Vec::new();
        while let Ok(job) = self.jobs.try_recv() {
            jobs.push(job);
        }
        jobs
    }

    /// Records where the generation the runtime is about to start for this
    /// session must send its answer.
    pub fn set_generation_answer(&mut self, answer: Sender<Result<String, String>>) {
        self.generation_answer = Some(answer);
    }

    /// Takes the pending generation answer, if a generation the runtime runs
    /// for this session has completed (or been cancelled).
    pub fn take_generation_answer(&mut self) -> Option<Sender<Result<String, String>>> {
        self.generation_answer.take()
    }

    /// Sends the user's request to the agent, or queues it while a turn (or
    /// the handshake) is in flight. A session whose agent is gone refuses.
    pub fn prompt(&mut self, text: String) -> PromptOutcome {
        if !self.ready || self.busy {
            if self.dead() {
                return PromptOutcome::Dead;
            }
            if self.queued.len() >= MAX_QUEUED_PROMPTS {
                self.queued.pop_front();
            }
            self.queued.push_back(text);
            return PromptOutcome::Queued;
        }
        self.send_next_prompt(text)
    }

    /// Whether the agent process is gone and this session can no longer be
    /// used (prompts refused, no further events expected).
    pub fn dead(&self) -> bool {
        // The transport reports its own death; nothing else can tell us.
        self.transport_dead
    }

    /// Feeds queued agent events through the session's state machine,
    /// returning what the runtime should act on. Call on every event pass
    /// (cheap when idle). Pops queued prompts onto the agent as turns end.
    pub fn advance(&mut self) -> Vec<SessionUpdate> {
        let mut updates = Vec::new();
        for event in self.transport.drain_events() {
            match event {
                AcpEvent::SessionReady => {
                    self.ready = true;
                    updates.push(SessionUpdate::Ready);
                    self.flush_queue();
                }
                AcpEvent::TurnDone { text, .. } => {
                    self.busy = false;
                    if !text.trim().is_empty() {
                        updates.push(SessionUpdate::Reply { text });
                    }
                    self.flush_queue();
                }
                AcpEvent::Error(msg) => {
                    // The agent answered the outstanding request with an
                    // error; it is idle again, and any queued asks continue.
                    self.busy = false;
                    updates.push(SessionUpdate::Error(msg));
                    self.flush_queue();
                }
                AcpEvent::ProcessGone(msg) => {
                    self.busy = false;
                    self.transport_dead = true;
                    updates.push(SessionUpdate::Gone(msg));
                }
                // Streamed content a chat does not need to echo: thinking,
                // tool-call titles, plans and proof-of-life ticks all matter
                // only to the generation console. A session's output channel
                // is the room, and only completed turns go there.
                AcpEvent::Chunk(_)
                | AcpEvent::Thought(_)
                | AcpEvent::ToolCall(_)
                | AcpEvent::Plan(_)
                | AcpEvent::Tick => {}
            }
        }
        updates
    }

    /// Sends `text` as the next prompt, marking the session busy.
    fn send_next_prompt(&mut self, text: String) -> PromptOutcome {
        if self.dead() {
            return PromptOutcome::Dead;
        }
        self.transport.send_prompt(&text);
        self.busy = true;
        PromptOutcome::Sent
    }

    /// Sends the front of the queue, if any, to the now-idle agent.
    fn flush_queue(&mut self) {
        if self.dead() || self.busy {
            return;
        }
        if let Some(text) = self.queued.pop_front() {
            self.send_next_prompt(text);
        }
    }
}

impl Drop for AiSession {
    fn drop(&mut self) {
        // Unblock a serve thread still waiting on a launch_splash_app whose
        // generation the runtime was running: the socket is about to close,
        // so the tool must not hang. Dropping `transport` (kills the agent)
        // and `_server` (closes the socket) happens right after.
        if let Some(answer) = self.generation_answer.take() {
            let _ = answer.send(Err(
                "this room's AI session ended before the app finished building".to_string()
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rendezvous at the heart of the session host: a tool call made on
    /// one thread parks until the "UI thread" drains the job, does the work,
    /// and answers — after which the caller sees the result. No sockets, no
    /// agent: this is the marshaling contract in isolation.
    #[test]
    fn a_tool_call_waits_for_the_ui_threads_answer() {
        let (jobs_tx, jobs_rx) = channel::<SessionJob>();
        let host: Arc<dyn AiHost> = Arc::new(SessionHost { jobs: jobs_tx });

        // A serve thread calls the tool; it must block until answered.
        let caller_host = host.clone();
        let caller = std::thread::spawn(move || {
            caller_host.launch_splash_app("a counter app").unwrap()
        });

        // The UI thread picks the job up and does the work.
        let job = jobs_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        match job {
            SessionJob::LaunchSplashApp { description, answer } => {
                assert_eq!(description, "a counter app");
                let summary = serde_json::json!({
                    "app_id": "counter",
                    "name": "Counter",
                    "status": "installed_and_running",
                });
                answer.send(Ok(summary.to_string())).unwrap();
            }
            _other => panic!("expected a launch job, got a different job"),
        }

        assert_eq!(
            caller.join().unwrap(),
            "{\"app_id\":\"counter\",\"name\":\"Counter\",\"status\":\"installed_and_running\"}"
        );
    }

    /// A refused job (no provider, another generation running) surfaces to
    /// the caller as an `Err`, never as a hang.
    #[test]
    fn a_refused_tool_call_reports_the_reason() {
        let (jobs_tx, jobs_rx) = channel::<SessionJob>();
        let host: Arc<dyn AiHost> = Arc::new(SessionHost { jobs: jobs_tx });

        let caller_host = host.clone();
        let caller = std::thread::spawn(move || {
            caller_host.send_room_message("hello room")
        });

        let job = jobs_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        match job {
            SessionJob::SendRoomMessage { text, answer } => {
                assert_eq!(text, "hello room");
                answer.send(Err("matrix is read-only right now".to_string())).unwrap();
            }
            _other => panic!("expected a message job, got a different job"),
        }

        assert_eq!(caller.join().unwrap(), Err("matrix is read-only right now".to_string()));
    }

    /// Dropping the host side (session end) makes an in-flight call fail
    /// rather than hang forever.
    #[test]
    fn a_call_in_flight_when_the_session_dies_fails() {
        let (jobs_tx, jobs_rx) = channel::<SessionJob>();
        let host: Arc<dyn AiHost> = Arc::new(SessionHost { jobs: jobs_tx });

        let caller_host = host.clone();
        let caller = std::thread::spawn(move || {
            caller_host.launch_splash_app("never answered")
        });

        // The job is drained but never answered — the session ends first.
        let job = jobs_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        drop(job);
        drop(host);
        drop(jobs_rx);

        let err = caller.join().unwrap().unwrap_err();
        assert!(!err.is_empty());
    }
}
