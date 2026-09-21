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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use makepad_widgets::SignalToUI;

use octos_cli::commands::acp::AcpCommand;

use crate::acp_client::AcpEvent;
use crate::mcp::McpServerConfig as RobrixMcpServerConfig;
use crate::prefs::AgentPrefs;
use crate::AgentTransport;

/// Keep both generation and room turns bounded independently of Octos's
/// ordinary interactive runtime, whose default is unlimited.
const MAX_ITERATIONS: u32 = 12;
const MAX_HOST_TOOL_CALLS: usize = 4;
const HOST_TOOL_TIMEOUT_SECS: u64 = 600;

enum Cmd {
    Prompt(String, u64),
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
    cancelled: tokio::sync::Notify,
}

#[derive(Default)]
struct ShutdownInner {
    requested: bool,
    generation: u64,
    agent: Option<Arc<AtomicBool>>,
}

impl Shutdown {
    fn set(&self, value: bool) {
        let mut inner = self.inner.lock().unwrap();
        inner.requested = value;
        if value { inner.generation = inner.generation.wrapping_add(1); }
        if let Some(flag) = &inner.agent {
            flag.store(value, Ordering::Release);
        }
        drop(inner);
        if value { self.cancelled.notify_waiters(); }
    }

    fn begin_prompt(&self) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        inner.requested = false;
        if let Some(flag) = &inner.agent { flag.store(false, Ordering::Release); }
        inner.generation
    }

    /// Takes ownership of the agent's flag, applying whatever was asked for
    /// while it didn't exist yet.
    fn adopt(&self, flag: Arc<AtomicBool>) {
        let mut inner = self.inner.lock().unwrap();
        flag.store(inner.requested, Ordering::Release);
        inner.agent = Some(flag);
    }

    fn check(&self, generation: u64) -> Result<(), String> {
        let inner = self.inner.lock().unwrap();
        if inner.requested || inner.generation != generation {
            Err("Agent request cancelled.".into())
        } else {
            Ok(())
        }
    }

    async fn wait_for_cancel(&self, generation: u64) {
        loop {
            let notified = self.cancelled.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.check(generation).is_err() { return; }
            notified.await;
        }
    }
}

/// In-process octos agent behind the same event interface as the ACP client.
pub struct EmbeddedOctos {
    events: Receiver<AcpEvent>,
    cmd_tx: Sender<Cmd>,
    shutdown: Arc<Shutdown>,
}

impl EmbeddedOctos {
    /// Protected room sessions use the live host registry directly, including
    /// on iOS; protected generation has no tools. Ordinary standalone calls
    /// use the upstream factory and its normal coding surface.
    pub fn start(
        workspace: &Path,
        prefs: &AgentPrefs,
        mcp_servers: &[RobrixMcpServerConfig],
        host_managed: bool,
        network_approval: Option<Arc<dyn crate::NetworkApproval>>,
        model_context: Option<a2app_core::information_flow::ContextId>,
        host_tools: Option<crate::mcp::McpServer>,
    ) -> Result<Self, String> {
        validate_backend_options(host_managed, model_context.is_some(), !mcp_servers.is_empty(), network_approval.is_some(), host_tools.is_some())?;
        if model_context.is_none() {
            std::fs::create_dir_all(workspace).ok();
        }
        let (evt_tx, events) = std::sync::mpsc::channel();
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let shutdown = Arc::new(Shutdown::default());
        let ws = workspace.to_path_buf();
        let prefs = prefs.clone();
        let sd = shutdown.clone();
        std::thread::spawn(move || {
            agent_thread(ws, prefs, host_managed, model_context, host_tools, cmd_rx, evt_tx, sd)
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
        let generation = self.shutdown.begin_prompt();
        let _ = self.cmd_tx.send(Cmd::Prompt(text.to_string(), generation));
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
    host_managed: bool,
    model_context: Option<a2app_core::information_flow::ContextId>,
    host_tools: Option<crate::mcp::McpServer>,
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
        .max_blocking_threads(MAX_HOST_TOOL_CALLS)
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

    let (mut agent, host_tools) = match rt.block_on(build_agent(
        &workspace,
        &prefs,
        &shutdown,
        host_managed,
        model_context,
        host_tools,
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
    while let Ok(Cmd::Prompt(text, generation)) = cmd_rx.recv() {
        if shutdown.check(generation).is_err() {
            send(&evt_tx, AcpEvent::TurnDone { stop_reason: "cancelled".into(), text: String::new() });
            continue;
        }
        if let Some(tools) = &host_tools {
            match tools.agent_for_turn(&agent, generation) {
                Ok(refreshed) => agent = refreshed,
                Err(error) => {
                    if shutdown.check(generation).is_err() {
                        send(&evt_tx, AcpEvent::TurnDone { stop_reason: "cancelled".into(), text: String::new() });
                    } else {
                        send(&evt_tx, AcpEvent::Error(error));
                    }
                    continue;
                }
            }
        }
        rt.block_on(run_turn(
            &agent,
            &shutdown,
            &evt_tx,
            &mut history,
            &text,
            generation,
        ));
    }
}

/// Reject unsupported contracts before loading config or starting a runtime.
fn validate_backend_options(
    room_agent: bool,
    protected: bool,
    has_mcp_servers: bool,
    needs_network_approval: bool,
    has_host_tools: bool,
) -> Result<(), String> {
    if room_agent && !protected {
        return Err("A room agent needs a registered private-data context.".into());
    }
    if protected {
        if room_agent && !has_host_tools {
            return Err("The room's host tools are unavailable.".into());
        }
        return Ok(());
    }
    if has_mcp_servers || has_host_tools {
        return Err("Upstream Octos's ordinary embedded runtime cannot accept per-session host tools. Use a protected agent context.".into());
    }
    if needs_network_approval {
        return Err("Upstream Octos's ordinary embedded runtime cannot enforce the requested network approval callback. Use a protected agent context.".into());
    }
    Ok(())
}

/// Uses the guarded assembly for private contexts, otherwise the ACP factory.
async fn build_agent(
    workspace: &Path,
    prefs: &AgentPrefs,
    shutdown: &Arc<Shutdown>,
    host_managed: bool,
    model_context: Option<a2app_core::information_flow::ContextId>,
    host_tools: Option<crate::mcp::McpServer>,
) -> Result<(Arc<octos_agent::Agent>, Option<HostTools>), String> {
    if let Some(context) = model_context {
        return build_protected_agent(prefs, shutdown, host_managed, context, host_tools).await;
    }
    let cwd = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let command = AcpCommand {
        cwd: Some(cwd.clone()),
        max_iterations: MAX_ITERATIONS,
        // Preserve the pane's provider/model selection while using upstream's
        // canonical factory for config, credentials, plugins and native tools.
        provider: crate::providers::session_provider(),
        model: crate::prefs::Backend::detect().model_override(prefs),
        ..Default::default()
    };
    let factory = command.factory().map_err(|e| e.to_string())?;
    // This is the public bare-Agent embedding seam. Never wipe or retry the
    // user's Octos data directory when canonical bootstrap reports an error.
    let built = factory.build(cwd).await.map_err(|e| e.to_string())?;
    finish_agent(built, shutdown).map(|agent| (agent, None))
}

thread_local! {
    static PRIVATE_TRACE: std::cell::RefCell<Option<tracing::subscriber::DefaultGuard>> = const { std::cell::RefCell::new(None) };
}

async fn build_protected_agent(
    prefs: &AgentPrefs,
    shutdown: &Arc<Shutdown>,
    room_agent: bool,
    context: a2app_core::information_flow::ContextId,
    host_tools: Option<crate::mcp::McpServer>,
) -> Result<(Arc<octos_agent::Agent>, Option<HostTools>), String> {
    let epoch = a2app_core::information_flow::context_epoch(&context)?;
    let flag = Arc::new(AtomicBool::new(false));
    shutdown.adopt(flag.clone());
    let provider = crate::model_transport::provider(prefs, context.clone(), flag.clone())?;
    let tools = if room_agent {
        Some(HostTools {
            registry: host_tools.ok_or("The room's host tools are unavailable.")?,
            check_context: Arc::new(move || a2app_core::information_flow::ensure_context_epoch(&context, epoch)),
            shutdown: shutdown.clone(),
            slots: Arc::new(tokio::sync::Semaphore::new(MAX_HOST_TOOL_CALLS)),
        })
    } else { None };
    // No filesystem backing exists, even when upstream memory code changes.
    let memory = octos_memory::EpisodeStore::in_memory()
        .map_err(|_| "Could not initialize isolated agent memory.")?;
    let config = octos_agent::AgentConfig {
        max_iterations: MAX_ITERATIONS, save_episodes: false, suppress_auto_send_files: true,
        format_after_edit: false, ..Default::default()
    };
    let prompt = if room_agent {
        "You are the room's assistant. Use only the host's advertised tools. Private data may be shared only where the host allows it."
    } else {
        "You create Robrix mini-apps. Return the complete app in a fenced splash code block as requested. You have no filesystem or research tools."
    };
    let agent = octos_agent::Agent::new(octos_core::AgentId::new("robrix-protected"), provider, octos_agent::ToolRegistry::new(), Arc::new(memory))
        .with_config(config).with_shutdown(flag).with_system_prompt(prompt.into());
    let agent = Arc::new(agent);
    #[cfg(feature = "persistent-guide")]
    agent.append_system_prompt(crate::SPLASH_GUIDE);
    Ok((agent, tools))
}

/// The live host registry is the only source of tools in a protected room.
#[derive(Clone)]
struct HostTools {
    registry: crate::mcp::McpServer,
    check_context: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    shutdown: Arc<Shutdown>,
    slots: Arc<tokio::sync::Semaphore>,
}

impl HostTools {
    fn check(&self, generation: u64) -> Result<(), String> {
        self.shutdown.check(generation)?;
        (self.check_context)()
    }

    fn agent_for_turn(&self, previous: &octos_agent::Agent, generation: u64) -> Result<Arc<octos_agent::Agent>, String> {
        self.check(generation)?;
        let specs = octos_llm::host::ToolsListResponse {
            tools: self.registry.tools().into_iter().map(|tool| octos_llm::ToolSpec {
                name: tool.name().into(), description: tool.description().into(), input_schema: tool.input_schema(),
            }).collect(),
        };
        specs.validate()?;
        let mut registry = octos_agent::ToolRegistry::new();
        for spec in specs.tools {
            registry.register(HostTool { spec, host: self.clone(), generation });
        }
        // Protected agents have only these explicit dependencies. Rebuilding
        // their immutable registry between turns picks up live tool changes;
        // history and the memory-only store remain owned by this session.
        let agent = octos_agent::Agent::new(octos_core::AgentId::new("robrix-protected"), previous.llm_provider(), registry, previous.memory_store())
            .with_config(previous.agent_config()).with_shutdown(previous.shutdown_signal())
            .with_system_prompt(previous.system_prompt_snapshot());
        Ok(Arc::new(agent))
    }
}

struct HostTool {
    spec: octos_llm::ToolSpec,
    host: HostTools,
    generation: u64,
}

#[async_trait::async_trait]
impl octos_agent::Tool for HostTool {
    fn name(&self) -> &str { &self.spec.name }
    fn description(&self) -> &str { &self.spec.description }
    fn input_schema(&self) -> serde_json::Value { self.spec.input_schema.clone() }
    fn execution_timeout_secs(&self) -> Option<u64> { Some(HOST_TOOL_TIMEOUT_SECS) }

    async fn execute(&self, arguments: &serde_json::Value) -> eyre::Result<octos_agent::ToolResult> {
        self.host.check(self.generation).map_err(eyre::Report::msg)?;
        octos_llm::host::validate_payload_size(arguments).map_err(eyre::Report::msg)?;
        let arguments = arguments.as_object().ok_or_else(|| eyre::eyre!("Tool arguments must be an object."))?.clone();
        let call = async {
            let permit = self.host.slots.clone().acquire_owned().await
                .map_err(|_| eyre::eyre!("Host tools are unavailable."))?;
            self.host.check(self.generation).map_err(eyre::Report::msg)?;
            let host = self.host.clone();
            let generation = self.generation;
            let name = self.spec.name.clone();
            let result = tokio::task::spawn_blocking(move || {
                // Cancellation drops the async waiter, but this permit stays
                // with the actual blocking work until it exits.
                let _permit = permit;
                host.check(generation)?;
                let tool = host.registry.tools().into_iter().find(|tool| tool.name() == name)
                    .ok_or("The requested host tool is unavailable.")?;
                let result = tool.call(&arguments);
                host.check(generation)?;
                Ok::<_, String>(result)
            }).await.map_err(|_| eyre::eyre!("Host tool failed."))?.map_err(eyre::Report::msg)?;
            self.host.check(self.generation).map_err(eyre::Report::msg)?;
            let (output, success) = match result { Ok(output) => (output, true), Err(output) => (output, false) };
            octos_llm::host::validate_payload_size(&output).map_err(eyre::Report::msg)?;
            Ok(octos_agent::ToolResult { output, success, ..Default::default() })
        };
        tokio::select! {
            biased;
            _ = self.host.shutdown.wait_for_cancel(self.generation) => Err(eyre::eyre!("Agent request cancelled.")),
            result = call => result,
        }
    }
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
    streamed_iteration: AtomicU64,
    shutdown: Arc<Shutdown>,
    generation: u64,
}

impl octos_agent::ProgressReporter for Reporter {
    /// Mirrors `progress_event_to_acp` in octos's own ACP command, so the
    /// console shows the same run whichever backend produced it.
    fn report(&self, event: octos_agent::ProgressEvent) {
        use octos_agent::ProgressEvent as E;
        if self.shutdown.check(self.generation).is_err() { return; }
        match event {
            E::StreamChunk { text, iteration } => {
                self.streamed_iteration.store(u64::from(iteration) + 1, Ordering::Release);
                send(&self.evt_tx, AcpEvent::Chunk(text));
            }
            // The loop emits streaming deltas AND a final full Response with
            // the same text; forward the Response only when nothing streamed
            // (non-streaming providers).
            E::Response { content, iteration } => {
                if self.streamed_iteration.load(Ordering::Acquire) != u64::from(iteration) + 1 {
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

/// Drive the canonical Agent and retain only this turn's returned rows.
/// Cancellation and truncated model output keep distinct terminal outcomes.
async fn run_turn(
    agent: &Arc<octos_agent::Agent>,
    shutdown: &Arc<Shutdown>,
    evt_tx: &Sender<AcpEvent>,
    history: &mut Vec<octos_core::Message>,
    text: &str,
    generation: u64,
) {
    // NOTE: the stale-cancel reset happens in send_prompt (UI side), BEFORE
    // the command is queued — so a cancel/drop arriving while the turn waits
    // in the queue is never clobbered here.
    agent.set_reporter(Arc::new(Reporter {
        evt_tx: evt_tx.clone(),
        streamed_iteration: AtomicU64::new(0),
        shutdown: shutdown.clone(),
        generation,
    }));

    let snapshot = history.clone();
    let process = agent.process_message(text, &snapshot, vec![]);
    let outcome = tokio::select! {
        biased;
        _ = shutdown.wait_for_cancel(generation) => Err(eyre::eyre!("Agent request cancelled.")),
        outcome = process => outcome,
    };
    let cancelled = shutdown.check(generation).is_err();

    match outcome {
        Ok(resp) => {
            let pending_approval = resp.pending_approval.is_some();
            let assistant_reply = retain_response(history, resp, cancelled);
            if pending_approval && !cancelled {
                send(evt_tx, AcpEvent::Error("This action requires Octos's interactive tool approval, which this embedded session cannot provide. The action was not executed.".into()));
                return;
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
            if let Some(incomplete) = e.downcast_ref::<octos_agent::IncompleteResponseError>() {
                retain_response(history, incomplete.partial.clone(), cancelled);
            }
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

/// Upstream returns this turn's rows, not the previous history snapshot.
fn retain_response(history: &mut Vec<octos_core::Message>, response: octos_agent::ConversationResponse, cancelled: bool) -> String {
    let assistant_reply = response.content;
    history.extend(response.messages);
    let already_retained = history.last().is_some_and(|message| {
        message.role == octos_core::MessageRole::Assistant && message.content == assistant_reply
    });
    if !cancelled && !assistant_reply.is_empty() && !already_retained {
        let mut message = octos_core::Message::assistant(assistant_reply.clone());
        message.reasoning_content = response.reasoning_content;
        history.push(message);
    }
    assistant_reply
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_agent::ProgressReporter;
    use std::sync::atomic::AtomicUsize;

    struct FixtureProvider { calls: AtomicUsize, truncate_first: bool }

    #[async_trait::async_trait]
    impl octos_llm::LlmProvider for FixtureProvider {
        async fn chat(&self, _: &[octos_core::Message], _: &[octos_llm::ToolSpec], _: &octos_llm::ChatConfig) -> eyre::Result<octos_llm::ChatResponse> {
            let first = self.calls.fetch_add(1, Ordering::AcqRel) == 0;
            Ok(octos_llm::ChatResponse {
                content: Some(if first { "partial answer" } else { "complete answer" }.into()),
                reasoning_content: None, tool_calls: Vec::new(), usage: Default::default(), provider_index: None,
                stop_reason: if first && self.truncate_first { octos_llm::StopReason::MaxTokens } else { octos_llm::StopReason::EndTurn },
            })
        }
        fn model_id(&self) -> &str { "embedded-fixture" }
        fn provider_name(&self) -> &str { "fixture" }
    }

    fn fixture_agent(shutdown: &Arc<Shutdown>, truncate_first: bool) -> Arc<octos_agent::Agent> {
        let flag = Arc::new(AtomicBool::new(false));
        shutdown.adopt(flag.clone());
        Arc::new(octos_agent::Agent::new(
            octos_core::AgentId::new("fixture"),
            Arc::new(FixtureProvider { calls: AtomicUsize::new(0), truncate_first }),
            octos_agent::ToolRegistry::new(),
            Arc::new(octos_memory::EpisodeStore::in_memory().unwrap()),
        ).with_config(octos_agent::AgentConfig { max_iterations: 2, save_episodes: false, ..Default::default() })
            .with_shutdown(flag).with_system_prompt("Fixture instruction.".into()))
    }

    struct FixtureTool {
        name: &'static str,
        content: &'static str,
        calls: Arc<AtomicUsize>,
        entered: Option<Arc<tokio::sync::Notify>>,
        release: Option<Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>>,
    }

    impl crate::mcp::Tool for FixtureTool {
        fn name(&self) -> &str { self.name }
        fn description(&self) -> &str { "Fixture host operation" }
        fn input_schema(&self) -> serde_json::Value { serde_json::json!({"type":"object"}) }
        fn call(&self, _: &serde_json::Map<String, serde_json::Value>) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            if let Some(entered) = &self.entered { entered.notify_one(); }
            if let Some(release) = &self.release {
                let (lock, condition) = &**release;
                let mut ready = lock.lock().unwrap();
                while !*ready { ready = condition.wait(ready).unwrap(); }
            }
            Ok(self.content.into())
        }
    }

    fn fixture_tools(shutdown: &Arc<Shutdown>, allowed: Arc<AtomicBool>) -> HostTools {
        HostTools {
            registry: crate::mcp::McpServer::new(), shutdown: shutdown.clone(),
            check_context: Arc::new(move || if allowed.load(Ordering::Acquire) { Ok(()) } else { Err("Context retired.".into()) }),
            slots: Arc::new(tokio::sync::Semaphore::new(MAX_HOST_TOOL_CALLS)),
        }
    }

    fn add_fixture_tool(tools: &HostTools, name: &'static str, content: &'static str, calls: &Arc<AtomicUsize>) {
        tools.registry.add_tool(FixtureTool { name, content, calls: calls.clone(), entered: None, release: None });
    }

    #[test]
    fn ordinary_and_protected_options_preserve_required_boundaries() {
        assert!(validate_backend_options(false, false, false, false, false).is_ok());
        assert!(validate_backend_options(false, false, true, false, false).is_err());
        assert!(validate_backend_options(false, false, false, true, false).is_err());
        assert!(validate_backend_options(true, false, false, false, true).is_err());
        assert!(validate_backend_options(true, true, false, false, false).is_err());
        assert!(validate_backend_options(true, true, true, true, true).is_ok());
        assert!(validate_backend_options(false, true, false, false, false).is_ok());
    }

    #[test]
    fn cancellation_before_build_and_next_prompt_never_revives_old_generation() {
        let shutdown = Shutdown::default();
        let old = shutdown.begin_prompt();
        shutdown.set(true);
        let flag = Arc::new(AtomicBool::new(false));
        shutdown.adopt(flag.clone());
        assert!(flag.load(Ordering::Acquire));
        let next = shutdown.begin_prompt();
        assert!(!flag.load(Ordering::Acquire));
        assert!(shutdown.check(old).is_err());
        assert!(shutdown.check(next).is_ok());
    }

    #[tokio::test]
    async fn protected_tools_refresh_and_recheck_the_live_registry_and_context() {
        let shutdown = Arc::new(Shutdown::default());
        let generation = shutdown.begin_prompt();
        let allowed = Arc::new(AtomicBool::new(true));
        let tools = fixture_tools(&shutdown, allowed.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        add_fixture_tool(&tools, "first", "old", &calls);
        let previous = fixture_agent(&shutdown, false);
        let agent = tools.agent_for_turn(&previous, generation).unwrap();
        assert!(agent.tool_registry().workspace_root().is_none());
        assert_eq!(agent.tool_registry().specs().len(), 1);
        assert!(Arc::ptr_eq(&agent.memory_store(), &previous.memory_store()));
        assert_eq!(agent.system_prompt_snapshot(), previous.system_prompt_snapshot());
        let tool = agent.tool_registry().get("first").unwrap();
        assert_eq!(tool.execution_timeout_secs(), Some(600));
        assert_eq!(tool.execute(&serde_json::json!({})).await.unwrap().output, "old");
        tools.registry.remove_tool("first");
        assert!(tool.execute(&serde_json::json!({})).await.is_err());
        add_fixture_tool(&tools, "first", "replacement", &calls);
        assert_eq!(tool.execute(&serde_json::json!({})).await.unwrap().output, "replacement");
        add_fixture_tool(&tools, "second", "new", &calls);
        assert_eq!(tools.agent_for_turn(&agent, generation).unwrap().tool_registry().specs().len(), 2);
        allowed.store(false, Ordering::Release);
        assert!(tool.execute(&serde_json::json!({})).await.is_err());
        assert_eq!(calls.load(Ordering::Acquire), 2);
    }

    #[tokio::test]
    async fn cancelled_blocking_tools_keep_their_capacity_until_the_call_exits() {
        struct Release(Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>);
        impl Release {
            fn open(&self) {
                let (lock, condition) = &*self.0;
                *lock.lock().unwrap() = true;
                condition.notify_all();
            }
        }
        impl Drop for Release { fn drop(&mut self) { self.open(); } }
        let shutdown = Arc::new(Shutdown::default());
        let generation = shutdown.begin_prompt();
        let mut tools = fixture_tools(&shutdown, Arc::new(AtomicBool::new(true)));
        tools.slots = Arc::new(tokio::sync::Semaphore::new(1));
        let calls = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Release(Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new())));
        tools.registry.add_tool(FixtureTool { name: "blocking", content: "done", calls: calls.clone(), entered: Some(entered.clone()), release: Some(release.0.clone()) });
        let agent = tools.agent_for_turn(&fixture_agent(&shutdown, false), generation).unwrap();
        let tool = agent.tool_registry().get("blocking").unwrap().clone();
        let first = tokio::spawn(async move { tool.execute(&serde_json::json!({})).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified()).await.unwrap();
        shutdown.set(true);
        assert!(tokio::time::timeout(std::time::Duration::from_secs(5), first).await.unwrap().unwrap().is_err());
        assert_eq!(tools.slots.available_permits(), 0);
        let next = shutdown.begin_prompt();
        let fresh = tools.agent_for_turn(&agent, next).unwrap();
        let next_tool = fresh.tool_registry().get("blocking").unwrap().clone();
        let second = tokio::spawn(async move { next_tool.execute(&serde_json::json!({})).await });
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::Acquire), 1);
        release.open();
        assert!(tokio::time::timeout(std::time::Duration::from_secs(5), second).await.unwrap().unwrap().is_ok());
        assert_eq!(calls.load(Ordering::Acquire), 2);
        assert!(agent.tool_registry().get("blocking").unwrap().execute(&serde_json::json!({})).await.is_err());
    }

    #[tokio::test]
    async fn truncated_model_output_is_an_error_with_retained_partial_history() {
        let shutdown = Arc::new(Shutdown::default());
        let generation = shutdown.begin_prompt();
        let agent = fixture_agent(&shutdown, true);
        let (events, receiver) = std::sync::mpsc::channel();
        let mut history = Vec::new();
        run_turn(&agent, &shutdown, &events, &mut history, "first", generation).await;
        let first: Vec<_> = receiver.try_iter().collect();
        assert!(first.iter().any(|event| matches!(event, AcpEvent::Error(message) if message.contains("incomplete"))));
        assert!(!first.iter().any(|event| matches!(event, AcpEvent::TurnDone { stop_reason, .. } if stop_reason == "end_turn")));
        assert!(history.iter().any(|message| message.role == octos_core::MessageRole::Assistant && message.content == "partial answer"));
        run_turn(&agent, &shutdown, &events, &mut history, "continue", generation).await;
        assert!(receiver.try_iter().any(|event| matches!(event, AcpEvent::TurnDone { stop_reason, text } if stop_reason == "end_turn" && text == "complete answer")));
        assert_eq!(history.iter().filter(|message| message.content == "partial answer").count(), 1);
    }

    #[test]
    fn progress_deduplication_is_per_iteration_and_old_reporters_stop_after_cancel() {
        let shutdown = Arc::new(Shutdown::default());
        let generation = shutdown.begin_prompt();
        let (events, receiver) = std::sync::mpsc::channel();
        let reporter = Reporter { evt_tx: events, streamed_iteration: AtomicU64::new(0), shutdown: shutdown.clone(), generation };
        reporter.report(octos_agent::ProgressEvent::StreamChunk { text: "first".into(), iteration: 1 });
        reporter.report(octos_agent::ProgressEvent::Response { content: "first".into(), iteration: 1 });
        reporter.report(octos_agent::ProgressEvent::Response { content: "second".into(), iteration: 2 });
        shutdown.set(true);
        shutdown.begin_prompt();
        reporter.report(octos_agent::ProgressEvent::StreamChunk { text: "stale".into(), iteration: 3 });
        let chunks: Vec<_> = receiver.try_iter().filter_map(|event| if let AcpEvent::Chunk(text) = event { Some(text) } else { None }).collect();
        assert_eq!(chunks, ["first", "second"]);
    }
}
