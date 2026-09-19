//! `embedded`: the octos agent running IN-PROCESS, no child process.
//!
//! Links the octos agent core (provider registry, tool registry + sandbox,
//! agent loop) directly and drives it on a dedicated thread that owns a tokio
//! runtime — the same shape `octos acp` itself uses, minus the process
//! boundary. This is the only local option on iOS, where `exec()` is
//! prohibited; on desktop it also removes the "install octos first" step
//! (only `~/.octos/config.json` — an `octos init` from any machine — is
//! needed for the provider).
//!
//! Protected sessions assemble a minimal agent with Robrix's guarded provider.
//! They do not import Octos plugins, fallback providers, persistent history,
//! embeddings, bootstrap files, or shell tools. Unprotected standalone callers
//! retain the upstream ACP factory.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use makepad_widgets::SignalToUI;

use octos_cli::commands::acp::AcpCommand;

use crate::acp_client::AcpEvent;
use crate::mcp::McpServerConfig as RobrixMcpServerConfig;
use crate::prefs::AgentPrefs;
use crate::AgentTransport;

/// Tool-call iterations allowed per turn. Lower than octos's own default of
/// 20: a turn here writes one Splash file and stops, so a budget that large
/// only buys a runaway loop more rope.
const MAX_ITERATIONS: u32 = 12;

enum Cmd {
    Prompt(String),
}

/// Cancellation, which has to work before the agent exists.
///
/// The agent's shutdown flag is created by octos inside `factory.build()` and
/// wired into the loop, so we can't hand ours in — we adopt theirs once it
/// arrives. Until then a cancel or a drop still has to be remembered, or
/// tearing down during the (network-bound) build would be silently ignored and
/// the first turn would run anyway. Both fields sit under one lock so adopting
/// can't race a concurrent cancel.
#[derive(Default)]
struct Shutdown {
    inner: std::sync::Mutex<ShutdownInner>,
}

#[derive(Default)]
struct ShutdownInner {
    requested: bool,
    agent: Option<Arc<AtomicBool>>,
}

impl Shutdown {
    fn set(&self, value: bool) {
        let mut inner = self.inner.lock().unwrap();
        inner.requested = value;
        if let Some(flag) = &inner.agent {
            flag.store(value, Ordering::Release);
        }
    }

    /// Takes ownership of the agent's flag, applying whatever was asked for
    /// while it didn't exist yet.
    fn adopt(&self, flag: Arc<AtomicBool>) {
        let mut inner = self.inner.lock().unwrap();
        flag.store(inner.requested, Ordering::Release);
        inner.agent = Some(flag);
    }

    fn is_set(&self) -> bool {
        self.inner.lock().unwrap().requested
    }
}

/// In-process octos agent behind the same event interface as the ACP client.
pub struct EmbeddedOctos {
    events: Receiver<AcpEvent>,
    cmd_tx: Sender<Cmd>,
    shutdown: Arc<Shutdown>,
}

impl EmbeddedOctos {
    /// `mcp_servers` are the stdio tool servers this session advertises to the
    /// agent — the same `McpServerConfig`s `AcpClient::spawn` puts in
    /// `session/new`. The embedded backend hands them to octos's ACP factory
    /// (`build_with_mcp`) instead, which connects them per session; on iOS the
    /// caller passes none (the agent cannot exec the relay child there).
    ///
    /// With a model context, room sessions get only host MCP tools; generation sessions have no tools. Unprotected standalone
    /// calls use the upstream factory and its normal coding surface.
    pub fn start(
        workspace: &Path,
        prefs: &AgentPrefs,
        mcp_servers: &[RobrixMcpServerConfig],
        host_managed: bool,
        network_approval: Option<Arc<dyn crate::NetworkApproval>>,
        model_context: Option<a2app_core::information_flow::ContextId>,
    ) -> Result<Self, String> {
        std::fs::create_dir_all(workspace).ok();
        let (evt_tx, events) = std::sync::mpsc::channel();
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let shutdown = Arc::new(Shutdown::default());
        let ws = workspace.to_path_buf();
        let prefs = prefs.clone();
        let sd = shutdown.clone();
        let servers = mcp_servers.to_vec();
        std::thread::spawn(move || {
            agent_thread(ws, prefs, servers, host_managed, network_approval, model_context, cmd_rx, evt_tx, sd)
        });
        Ok(Self { events, cmd_tx, shutdown })
    }
}

impl AgentTransport for EmbeddedOctos {
    fn drain_events(&mut self) -> Vec<AcpEvent> {
        let mut out = Vec::new();
        while let Ok(e) = self.events.try_recv() {
            out.push(e);
        }
        out
    }

    fn send_prompt(&mut self, text: &str) {
        // Clear a stale cancel flag HERE (not inside the turn) so a cancel or
        // drop that lands after this queue-up still wins — mirroring octos
        // acp's own dispatch-loop reset ordering.
        self.shutdown.set(false);
        let _ = self.cmd_tx.send(Cmd::Prompt(text.to_string()));
    }

    fn cancel(&mut self) {
        // The agent loop checks this at each iteration/stream-chunk boundary.
        self.shutdown.set(true);
    }

    fn desc(&self) -> &str {
        "octos (in-process)"
    }
}

impl Drop for EmbeddedOctos {
    fn drop(&mut self) {
        // Without this, dropping the client mid-turn (stall watchdog, app
        // teardown) leaves the agent turn running detached — burning tokens
        // invisibly for up to max_iterations. The flag aborts it at the next
        // loop/stream checkpoint; the thread then sees the closed command
        // channel and exits, taking the runtime with it.
        self.shutdown.set(true);
    }
}

fn send(evt_tx: &Sender<AcpEvent>, event: AcpEvent) {
    let _ = evt_tx.send(event);
    SignalToUI::set_ui_signal();
}

/// The agent thread: build once, then serve prompt turns until the client
/// (and thus the command channel) is dropped.
fn agent_thread(
    workspace: PathBuf,
    prefs: AgentPrefs,
    mcp_servers: Vec<RobrixMcpServerConfig>,
    host_managed: bool,
    network_approval: Option<Arc<dyn crate::NetworkApproval>>,
    model_context: Option<a2app_core::information_flow::ContextId>,
    cmd_rx: Receiver<Cmd>,
    evt_tx: Sender<AcpEvent>,
    shutdown: Arc<Shutdown>,
) {
    // Debug builds only: surface octos's own tracing (MCP connect/discovery,
    // provider resolution) on stderr so an embedded-agent failure is visible
    // in the `cargo run` console instead of vanishing into a dead subscriber.
    // Robrix itself doesn't use tracing, so this prints octos lines only.
    #[cfg(debug_assertions)]
    if model_context.is_none() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_writer(std::io::stderr)
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| {
                        "octos_agent::mcp=debug,octos_cli=info,octos=info".into()
                    }),
            )
            .try_init();
    }

    // Dependency diagnostics may contain model content. Keep protected tasks
    // off global tracing subscribers, including spawned runtime worker tasks.
    let protected = model_context.is_some();
    let _private_trace = protected.then(|| tracing::subscriber::set_default(tracing::subscriber::NoSubscriber::default()));
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .on_thread_start(move || {
            if protected {
                PRIVATE_TRACE.with(|guard| *guard.borrow_mut() = Some(tracing::subscriber::set_default(tracing::subscriber::NoSubscriber::default())));
            }
        })
        .on_thread_stop(|| { PRIVATE_TRACE.with(|guard| { guard.borrow_mut().take(); }); })
        .worker_threads(2)
        // Deep agent futures; octos's own entrypoints use an 8MB stack.
        .thread_stack_size(8 * 1024 * 1024)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            send(&evt_tx, AcpEvent::ProcessGone(format!("tokio runtime: {e}")));
            return;
        }
    };

    let empty_memory = if protected {
        match tempfile::Builder::new().prefix("guarded_empty_").tempdir_in(&workspace) {
            Ok(directory) => Some(directory),
            Err(_) => {
                send(&evt_tx, AcpEvent::ProcessGone("Could not create isolated agent memory.".into()));
                return;
            }
        }
    } else { None };

    let agent = match rt.block_on(build_agent(
        &workspace,
        &prefs,
        &shutdown,
        &mcp_servers,
        host_managed,
        model_context,
        empty_memory.as_ref().map(|directory| directory.path()),
    )) {
        Ok(agent) => agent,
        Err(e) => {
            send(&evt_tx, AcpEvent::ProcessGone(e));
            return;
        }
    };

    send(&evt_tx, AcpEvent::SessionReady);

    // Conversation history across turns (repair prompts rely on it), kept by
    // octos's exact append rules (see run_turn).
    let mut history: Vec<octos_core::Message> = Vec::new();

    // Blocking command loop OUTSIDE the runtime: recv() parks this thread;
    // each turn runs to completion on the runtime.
    while let Ok(Cmd::Prompt(text)) = cmd_rx.recv() {
        rt.block_on(run_turn(
            &agent,
            &shutdown,
            &evt_tx,
            &mut history,
            &text,
            network_approval.as_ref(),
        ));
    }
}

/// Uses the guarded assembly for private contexts, otherwise the ACP factory.
async fn build_agent(
    workspace: &Path,
    prefs: &AgentPrefs,
    shutdown: &Arc<Shutdown>,
    mcp_servers: &[RobrixMcpServerConfig],
    host_managed: bool,
    model_context: Option<a2app_core::information_flow::ContextId>,
    empty_memory: Option<&Path>,
) -> Result<Arc<octos_agent::Agent>, String> {
    if let Some(context) = model_context {
        return build_protected_agent(prefs, shutdown, mcp_servers, host_managed, context, empty_memory.ok_or("Missing isolated agent memory.")?).await;
    }
    let cwd = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let command = AcpCommand {
        cwd: Some(cwd.clone()),
        max_iterations: MAX_ITERATIONS,
        // The pane's picks, honoured HERE too. The child process gets them as
        // `--provider` / `--model` on its command line; without passing them
        // the embedded agent read config.json and quietly ignored both, so
        // choosing a provider in the Providers page changed nothing and the
        // page still reported it as the one in use.
        //
        // `model_override`, NOT `prefs.model`: the saved pick is global and
        // may belong to a provider that isn't this one. See its docs — passing
        // it blindly is what produced "invalid temperature: only 1 is allowed
        // for this model" against the Kimi coding plan.
        provider: crate::providers::session_provider(),
        model: crate::prefs::Backend::detect().model_override(prefs),
        // Host-managed sessions (the room's long-lived agent) run Robrix's
        // session profile: octos's registry is emptied of native tools, so
        // it can only call tools Robrix advertised in `mcp_servers` —
        // every one mediated by Robrix. The one-shot app-generation agent
        // (host_managed = false) keeps the default `coding` surface.
        profile: if host_managed {
            Some(crate::robrix_session_profile()?)
        } else {
            None
        },
        ..Default::default()
    };

    let factory = command.factory().map_err(|e| e.to_string())?;

    // Deliberately NO wipe-and-retry on failure. An earlier version cleared the
    // data dir and tried again, to recover from an episodes.redb truncated by a
    // crash — safe only while that dir was our own private scratch. It is the
    // user's own ~/.octos now, and their episode history is not ours to delete.
    //
    // The Robrix tool servers this session advertised are translated into
    // octos config and handed to `build_with_mcp`, exactly the per-session
    // `mcpServers` path the `octos acp` child process serves.
    let octos_servers: Vec<octos_agent::McpServerConfig> = mcp_servers
        .iter()
        .map(|server| octos_agent::McpServerConfig {
            command: Some(server.command.clone()),
            args: server.args.clone(),
            env: std::collections::HashMap::new(),
            url: None,
            headers: std::collections::HashMap::new(),
            oauth: false,
            scopes: Vec::new(),
            concurrency_class: None,
            // The relay's tools (app generation) run minutes-long by design;
            // octos's 60s operator default would cancel them mid-build.
            tool_call_timeout_secs: Some(600),
        })
        .collect();
    let built = factory
        .build_with_mcp(cwd, &octos_servers)
        .await
        .map_err(|e| e.to_string())?;
    finish_agent(built, shutdown)
}

thread_local! {
    static PRIVATE_TRACE: std::cell::RefCell<Option<tracing::subscriber::DefaultGuard>> = const { std::cell::RefCell::new(None) };
}

async fn build_protected_agent(
    prefs: &AgentPrefs,
    shutdown: &Arc<Shutdown>,
    mcp_servers: &[RobrixMcpServerConfig],
    host_managed: bool,
    context: a2app_core::information_flow::ContextId,
    empty_memory: &Path,
) -> Result<Arc<octos_agent::Agent>, String> {
    let flag = Arc::new(AtomicBool::new(false));
    shutdown.adopt(flag.clone());
    let provider = crate::model_transport::provider(prefs, context, flag.clone())?;
    let mut tools = octos_agent::ToolRegistry::new();
    if host_managed {
        // Native web_fetch resolves DNS before host approval and owns its
        // socket. Protected sessions may only use host-mediated MCP tools.
        let servers: Vec<_> = mcp_servers.iter().map(|server| octos_agent::McpServerConfig {
            command: Some(server.command.clone()), args: server.args.clone(),
            env: std::collections::HashMap::new(), url: None, headers: std::collections::HashMap::new(),
            oauth: false, scopes: Vec::new(), concurrency_class: None, tool_call_timeout_secs: Some(600),
        }).collect();
        if !servers.is_empty() {
            octos_agent::McpClient::start(&servers).await
                .map_err(|_| "Could not connect the host's agent tools.")?.register_tools(&mut tools);
        }
    }
    // Octos requires an EpisodeStore handle even with history disabled. A
    // fresh per-session directory prevents reading anyone else's old memory;
    // save_episodes=false and no embedder prevent storing private content.
    let memory = octos_memory::EpisodeStore::open(empty_memory).await
        .map_err(|_| "Could not initialize isolated agent memory.")?;
    let config = octos_agent::AgentConfig {
        max_iterations: MAX_ITERATIONS, save_episodes: false, suppress_auto_send_files: true,
        format_after_edit: false, ..Default::default()
    };
    let prompt = if host_managed {
        "You are the room's assistant. Use only the host's advertised tools. Private data may be shared only where the host allows it."
    } else {
        "You create Robrix mini-apps. Return the complete app in a fenced splash code block as requested. You have no filesystem or research tools."
    };
    let agent = octos_agent::Agent::new(octos_core::AgentId::new("robrix-protected"), provider, tools, Arc::new(memory))
        .with_config(config).with_shutdown(flag).with_system_prompt(prompt.into());
    let agent = Arc::new(agent);
    #[cfg(feature = "persistent-guide")]
    agent.append_system_prompt(crate::SPLASH_GUIDE);
    Ok(agent)
}

/// Adopts the agent's shutdown flag and applies our own additions.
fn finish_agent(
    built: (Arc<octos_agent::Agent>, Arc<AtomicBool>),
    shutdown: &Arc<Shutdown>,
) -> Result<Arc<octos_agent::Agent>, String> {
    let (agent, flag) = built;
    // From here a cancel reaches the running loop — including one that arrived
    // while the build was still in flight.
    shutdown.adopt(flag);

    // With persistent-guide the guide lives in the system prompt for the whole
    // session (the in-process analogue of the .octos/AGENTS.md bootstrap
    // file), and per-turn prompts go slim. `append_system_prompt` takes &self,
    // so this still works on octos's already-built agent.
    #[cfg(feature = "persistent-guide")]
    {
        agent.append_system_prompt(crate::SPLASH_GUIDE);
        crate::skills::mark_deployed();
    }

    Ok(agent)
}

/// Per-turn reporter: octos's own StreamChunk/Response dedupe, reduced to the
/// pipeline's event vocabulary.
struct Reporter {
    evt_tx: Sender<AcpEvent>,
    streamed: AtomicBool,
}

impl octos_agent::ProgressReporter for Reporter {
    /// Mirrors `progress_event_to_acp` in octos's own ACP command, so the
    /// console shows the same run whichever backend produced it.
    fn report(&self, event: octos_agent::ProgressEvent) {
        use octos_agent::ProgressEvent as E;
        match event {
            E::StreamChunk { text, .. } => {
                self.streamed.store(true, Ordering::Release);
                send(&self.evt_tx, AcpEvent::Chunk(text));
            }
            // The loop emits streaming deltas AND a final full Response with
            // the same text; forward the Response only when nothing streamed
            // (non-streaming providers).
            E::Response { content, .. } => {
                if !self.streamed.load(Ordering::Acquire) {
                    send(&self.evt_tx, AcpEvent::Chunk(content));
                }
            }
            // THE reasoning text. Dropping this is what made an embedded run
            // look hung: a thinking model — Kimi's k3 has thinking always on —
            // spends most of a turn emitting these and nothing else, so the UI
            // sat on one status with an empty console until the code finally
            // started. octos's ACP path maps it to `agent_thought_chunk`, which
            // is where the child process gets its 💭 from.
            E::ReasoningChunk { text, .. } => {
                send(&self.evt_tx, AcpEvent::Thought(text));
            }
            E::ToolStarted { name, tool_id, .. } => {
                send(&self.evt_tx, AcpEvent::ToolCall { id: tool_id, title: name });
            }
            // The terminal status of a tool call. Needed to close the live
            // card for tools executed by the agent,
            // which Robrix does not itself execute and so cannot resolve.
            E::ToolCompleted { tool_id, success, output_preview, .. } => {
                send(
                    &self.evt_tx,
                    AcpEvent::ToolCallDone { id: tool_id, ok: success, summary: output_preview },
                );
            }
            // Nothing to render, but they are proof the agent is alive, and
            // the pipeline's stall watchdog measures the gap since the LAST
            // event of any kind. Dropping them meant a long think looked
            // identical to a dead provider, and a turn that thought for longer
            // than the stall timeout was killed for being slow.
            E::Thinking { .. }
            | E::TaskStarted { .. }
            | E::LlmStatus { .. }
            | E::ToolProgress { .. }
            | E::FileModified { .. }
            | E::PlanUpdated { .. }
            | E::TokenUsage { .. }
            | E::CostUpdate { .. }
            | E::StreamDone { .. }
            | E::StreamRetry { .. } => send(&self.evt_tx, AcpEvent::Tick),
            _ => {}
        }
    }
}

/// Adapts Robrix's blocking [`crate::NetworkApproval`] to octos's async
/// per-turn network-access requester. The approval itself runs on a blocking
/// thread, so the agent's tokio worker is never parked on the UI while the
/// user reads the prompt.
struct RobrixNetworkRequester {
    approval: Arc<dyn crate::NetworkApproval>,
}

#[async_trait::async_trait]
impl octos_agent::tools::NetworkAccessRequester for RobrixNetworkRequester {
    async fn request_network_access(
        &self,
        request: octos_agent::tools::NetworkAccessRequest,
    ) -> octos_agent::tools::NetworkAccessDecision {
        let approval = self.approval.clone();
        let tool = request.tool_name;
        let host = request.host;
        let url = request.url;
        let allowed = tokio::task::spawn_blocking(move || approval.approve(&tool, &host, &url))
            .await
            .unwrap_or_else(|_| Err("network approval channel closed".to_string()))
            .is_ok();
        if allowed {
            octos_agent::tools::NetworkAccessDecision::Allow
        } else {
            octos_agent::tools::NetworkAccessDecision::Deny
        }
    }
}

/// One prompt turn, following `octos acp`'s run_prompt_turn to the letter:
/// stale-cancel reset before the turn, cancelled-Err mapped to a cancel (not
/// an error), and the two-guard history append.
async fn run_turn(
    agent: &Arc<octos_agent::Agent>,
    shutdown: &Arc<Shutdown>,
    evt_tx: &Sender<AcpEvent>,
    history: &mut Vec<octos_core::Message>,
    text: &str,
    network_approval: Option<&Arc<dyn crate::NetworkApproval>>,
) {
    // NOTE: the stale-cancel reset happens in send_prompt (UI side), BEFORE
    // the command is queued — so a cancel/drop arriving while the turn waits
    // in the queue is never clobbered here.
    agent.set_reporter(Arc::new(Reporter {
        evt_tx: evt_tx.clone(),
        streamed: AtomicBool::new(false),
    }));

    let snapshot = history.clone();
    let process = agent.process_message(text, &snapshot, vec![]);
    // The legacy standalone factory can supply a native network callback.
    // Protected sessions instead execute every tool through the host MCP
    // server and pass None here.
    let outcome = match network_approval {
        Some(approval) => {
            let requester: Arc<dyn octos_agent::tools::NetworkAccessRequester> =
                Arc::new(RobrixNetworkRequester { approval: approval.clone() });
            octos_agent::tools::NETWORK_ACCESS_CTX
                .scope(requester, process)
                .await
        }
        None => process.await,
    };
    let cancelled = shutdown.is_set();

    match outcome {
        Ok(resp) => {
            let assistant_reply = resp.content.clone();
            history.extend(resp.messages);
            let already = matches!(
                history.last(),
                Some(last) if last.role == octos_core::MessageRole::Assistant
                    && last.content == assistant_reply
            );
            if !cancelled && !assistant_reply.is_empty() && !already {
                history.push(octos_core::Message {
                    role: octos_core::MessageRole::Assistant,
                    content: assistant_reply.clone(),
                    media: vec![],
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    client_message_id: None,
                    thread_id: None,
                    timestamp: chrono::Utc::now(),
                });
            }
            let stop_reason = if cancelled { "cancelled" } else { "end_turn" };
            send(
                evt_tx,
                AcpEvent::TurnDone {
                    stop_reason: stop_reason.to_string(),
                    text: if cancelled { String::new() } else { assistant_reply },
                },
            );
        }
        Err(e) => {
            if cancelled {
                send(
                    evt_tx,
                    AcpEvent::TurnDone { stop_reason: "cancelled".to_string(), text: String::new() },
                );
            } else {
                send(evt_tx, AcpEvent::Error(format!("agent turn failed: {e}")));
            }
        }
    }
}
