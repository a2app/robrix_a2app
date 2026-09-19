//! The parent-owned model and tool boundary for a confined ACP process.
//!
//! One connection is bound to one activation. The child receives no credentials
//! or policy authority, and every request rechecks the host's live context.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, atomic::{AtomicBool, AtomicUsize, Ordering}};

use a2app_core::information_flow::{self as flow, ContextId};
use octos_llm::{LlmProvider, ToolSpec, host::*};
use serde_json::Value;
use tokio::sync::{mpsc, watch, Semaphore};

use crate::{AgentTransport, acp_client::AcpClient, mcp::McpServer, prefs::AgentPrefs};

const MAX_PENDING: usize = 8;
const MAX_ACTIVE: usize = 4;
type Check = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;
type Reply = Box<dyn FnOnce(Result<Value, String>) + Send>;

struct Request {
    key: String,
    bytes: usize,
    method: String,
    params: Value,
    generation: u64,
    reply: Reply,
}

pub(crate) struct HostBroker {
    config: HostConfig,
    provider: Arc<dyn LlmProvider>,
    tools: McpServer,
    check: Check,
    stopped: Arc<AtomicBool>,
    ready: AtomicBool,
    active: AtomicBool,
    generation: watch::Sender<u64>,
    requests: Mutex<Option<mpsc::Sender<Request>>>,
    pending: Mutex<HashSet<String>>,
    pending_bytes: AtomicUsize,
    /// Blocking calls keep their permit after their async waiter is cancelled.
    /// This bounds detached work across repeated cancel/restart cycles.
    tool_slots: Arc<Semaphore>,
}

impl HostBroker {
    #[cfg(test)]
    pub(crate) fn for_protocol_test() -> Arc<Self> {
        tests::protocol_fixture()
    }

    fn new(prefs: &AgentPrefs, context: ContextId, room_agent: bool, tools: Option<McpServer>) -> Result<Arc<Self>, String> {
        let epoch = flow::context_epoch(&context)?;
        let stopped = Arc::new(AtomicBool::new(false));
        let provider = crate::model_transport::provider(prefs, context.clone(), stopped.clone())?;
        let check: Check = Arc::new(move || flow::ensure_context_epoch(&context, epoch));
        #[allow(unused_mut)]
        let mut system_prompt = if room_agent {
            "You are the room's assistant. Use only the host's advertised tools. Private data may be shared only where the host allows it.".to_string()
        } else {
            "You create Robrix mini-apps. Return the complete app in a fenced splash code block as requested. You have no filesystem or research tools.".to_string()
        };
        let tools = if room_agent { tools.ok_or("The room's host tools are unavailable.")? } else { McpServer::new() };
        #[cfg(feature = "persistent-guide")]
        { system_prompt.push('\n'); system_prompt.push_str(crate::SPLASH_GUIDE); }
        Self::with_provider(provider, tools, check, stopped, system_prompt)
    }

    fn with_provider(provider: Arc<dyn LlmProvider>, tools: McpServer, check: Check, stopped: Arc<AtomicBool>, system_prompt: String) -> Result<Arc<Self>, String> {
        let config = HostConfig {
            version: VERSION,
            model: HostModel {
                model_id: provider.model_id().into(), provider_name: provider.provider_name().into(),
                context_window: provider.context_window(), max_output_tokens: provider.max_output_tokens(),
            },
            system_prompt,
        };
        config.validate()?;
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().max_blocking_threads(MAX_ACTIVE)
            .build().map_err(|_| "Could not start the host broker.")?;
        let (tx, mut rx) = mpsc::channel::<Request>(MAX_PENDING);
        let (generation, mut cancellation) = watch::channel(0);
        let broker = Arc::new(Self {
            config, provider, tools, check, stopped, ready: AtomicBool::new(false), active: AtomicBool::new(false), generation,
            requests: Mutex::new(Some(tx)), pending: Mutex::new(HashSet::new()), pending_bytes: AtomicUsize::new(0),
            tool_slots: Arc::new(Semaphore::new(MAX_ACTIVE)),
        });
        let weak = Arc::downgrade(&broker);
        std::thread::Builder::new().name("a2app-host-broker".into()).spawn(move || runtime.block_on(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    changed = cancellation.changed() => {
                        if changed.is_err() || weak.upgrade().is_none_or(|broker| broker.stopped.load(Ordering::Acquire)) { break; }
                    }
                    _ = tasks.join_next(), if !tasks.is_empty() => {}
                    request = rx.recv(), if tasks.len() < MAX_ACTIVE => {
                        let Some(request) = request else { break };
                        let Some(broker) = weak.upgrade() else { break };
                        tasks.spawn(async move {
                            let mut cancelled = broker.generation.subscribe();
                            let result = if *cancelled.borrow() != request.generation {
                                Err("Agent request cancelled.".into())
                            } else {
                                tokio::select! {
                                    biased;
                                    _ = cancelled.changed() => Err("Agent request cancelled.".into()),
                                    result = broker.dispatch(&request.method, request.params, request.generation) => result,
                                }
                            };
                            broker.pending.lock().unwrap().remove(&request.key);
                            broker.pending_bytes.fetch_sub(request.bytes, Ordering::AcqRel);
                            (request.reply)(result);
                        });
                    }
                }
            }
            tasks.abort_all();
        })).map_err(|_| "Could not start the host broker thread.")?;
        Ok(broker)
    }

    pub(crate) fn config(&self) -> &HostConfig { &self.config }

    pub(crate) fn accept_handshake(&self, result: &Value) -> Result<(), String> {
        let capabilities: HostCapabilities = serde_json::from_value(result["agentCapabilities"]["_meta"][CAPABILITY_KEY].clone())
            .map_err(|_| "This agent does not support Octos's protected host broker. Install the host-managed Octos branch.")?;
        if capabilities.version != VERSION || !capabilities.confined || capabilities.sandbox.is_empty() {
            return Err("The agent did not establish the required confined host broker.".into());
        }
        (self.check)()?;
        self.ready.store(true, Ordering::Release);
        Ok(())
    }

    pub(crate) fn begin_turn(&self) -> Result<(), String> {
        self.ensure_live()?;
        if self.active.swap(true, Ordering::AcqRel) { return Err("The agent already has an active turn.".into()); }
        Ok(())
    }

    pub(crate) fn cancel(&self) {
        self.active.store(false, Ordering::Release);
        self.generation.send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.requests.lock().unwrap().take();
        self.cancel();
    }

    fn ensure_live(&self) -> Result<(), String> {
        if self.stopped.load(Ordering::Acquire) || !self.ready.load(Ordering::Acquire) {
            return Err("The protected agent connection is not active.".into());
        }
        (self.check)()
    }

    /// Bounded, nonblocking admission keeps the ACP reader free to process
    /// cancellation and avoids creating a thread for every untrusted request.
    pub(crate) fn submit(&self, id: &Value, method: &str, params: Value, bytes: usize, reply: Reply) -> Result<(), String> {
        self.ensure_live()?;
        if !matches!(method, MODEL_METHOD | TOOLS_LIST_METHOD | TOOLS_CALL_METHOD) {
            return Err("Unsupported host broker method.".into());
        }
        if method != TOOLS_LIST_METHOD && !self.active.load(Ordering::Acquire) { return Err("The agent has no active turn.".into()); }
        if !(id.is_u64() || id.as_str().is_some_and(|id| id.len() <= 128)) { return Err("Invalid broker request identifier.".into()); }
        let key = id.to_string();
        let mut pending = self.pending.lock().unwrap();
        if pending.len() >= MAX_PENDING || !pending.insert(key.clone()) { return Err("Too many or duplicate host broker requests.".into()); }
        if self.pending_bytes.fetch_update(Ordering::AcqRel, Ordering::Acquire,
            |pending| pending.checked_add(bytes).filter(|total| *total <= 2 * MAX_FRAME_BYTES)).is_err() {
            pending.remove(&key);
            return Err("Host broker requests exceeded the byte limit.".into());
        }
        let request = Request { key: key.clone(), bytes, method: method.into(), params, generation: *self.generation.borrow(), reply };
        if self.requests.lock().unwrap().as_ref().is_none_or(|tx| tx.try_send(request).is_err()) {
            pending.remove(&key);
            self.pending_bytes.fetch_sub(bytes, Ordering::AcqRel);
            return Err("The host broker is unavailable or busy.".into());
        }
        Ok(())
    }

    async fn dispatch(self: &Arc<Self>, method: &str, params: Value, generation: u64) -> Result<Value, String> {
        self.ensure_live()?;
        if method != TOOLS_LIST_METHOD && !self.active.load(Ordering::Acquire) { return Err("The agent has no active turn.".into()); }
        if *self.generation.borrow() != generation { return Err("Agent request cancelled.".into()); }
        let result = match method {
            MODEL_METHOD => {
                let request: ModelRequest = serde_json::from_value(params).map_err(|_| "Invalid host model request.")?;
                request.validate()?;
                let response = self.provider.chat(&request.messages, &request.tools, &request.config).await
                    .map_err(|error| error.to_string())?;
                serde_json::to_value(response).map_err(|_| "Invalid model response.")?
            }
            TOOLS_LIST_METHOD => {
                serde_json::from_value::<ToolsListRequest>(params).map_err(|_| "Invalid host tool list request.")?;
                let tools = self.tools.tools().into_iter().map(|tool| ToolSpec {
                    name: tool.name().into(), description: tool.description().into(), input_schema: tool.input_schema(),
                }).collect();
                let response = ToolsListResponse { tools };
                response.validate()?;
                serde_json::to_value(response).map_err(|_| "Invalid host tool list.")?
            }
            TOOLS_CALL_METHOD => {
                let request: ToolCallRequest = serde_json::from_value(params).map_err(|_| "Invalid host tool call.")?;
                let Value::Object(arguments) = request.arguments else { return Err("Tool arguments must be an object.".into()); };
                // A dropped JoinHandle does not stop a blocking call. Acquire
                // before spawning, then hold the permit inside the closure so
                // cancellation cannot admit an unbounded queue behind it.
                let permit = self.tool_slots.clone().acquire_owned().await
                    .map_err(|_| "Host tools are unavailable.")?;
                self.ensure_live()?;
                if *self.generation.borrow() != generation { return Err("Agent request cancelled.".into()); }
                let tool = self.tools.tools().into_iter().find(|tool| tool.name() == request.name)
                    .ok_or("The requested host tool is unavailable.")?;
                let broker = self.clone();
                let response = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    broker.ensure_live()?;
                    if !broker.active.load(Ordering::Acquire) || *broker.generation.borrow() != generation { return Err("Agent request cancelled.".to_string()); }
                    Ok(match tool.call(&arguments) {
                        Ok(content) => ToolCallResponse { content, is_error: false },
                        Err(content) => ToolCallResponse { content, is_error: true },
                    })
                }).await.map_err(|_| "Host tool failed.")??;
                serde_json::to_value(response).map_err(|_| "Invalid host tool response.")?
            }
            _ => return Err("Unsupported host broker method.".into()),
        };
        self.ensure_live()?;
        if *self.generation.borrow() != generation { return Err("Agent request cancelled.".into()); }
        validate_payload_size(&result)?;
        Ok(result)
    }
}

fn executable(command: &str) -> Result<PathBuf, String> {
    let command = command.trim().strip_suffix(" acp").unwrap_or(command.trim());
    let path = Path::new(command);
    let candidates = if path.components().count() > 1 {
        vec![path.to_path_buf()]
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).map(|dir| dir.join(path)).collect()
    };
    candidates.into_iter().find_map(|path| path.is_file().then(|| path.canonicalize().ok()).flatten())
        .ok_or_else(|| "Install Octos's host-managed branch, or set ROBRIX_AGENT_CMD to the local Octos executable followed by acp.".into())
}

pub(crate) fn start(prefs: &AgentPrefs, context: ContextId, room_agent: bool, tools: Option<McpServer>) -> Result<Box<dyn AgentTransport>, String> {
    let command = crate::providers::agent_command().unwrap_or_else(|| "octos".into());
    let executable = executable(&command)?;
    let broker = HostBroker::new(prefs, context, room_agent, tools)?;
    AcpClient::spawn_protected(&executable, broker).map(|client| Box::new(client) as Box<dyn AgentTransport>)
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::{Message, MessageRole, ToolCall};
    use octos_llm::{ChatConfig, ChatResponse, StopReason, TokenUsage};
    use serde_json::json;
    use std::time::{Duration, Instant};

    struct Model {
        calls: AtomicUsize,
        wait: AtomicBool,
        tool_loop: bool,
        seen_tools: Mutex<Vec<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for Model {
        async fn chat(&self, messages: &[Message], tools: &[ToolSpec], _: &ChatConfig) -> eyre::Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            self.seen_tools.lock().unwrap().push(tools.iter().map(|tool| tool.name.clone()).collect());
            if self.wait.load(Ordering::Acquire) { std::future::pending::<()>().await; }
            let call = self.tool_loop && !messages.iter().any(|message| message.role == MessageRole::Tool);
            Ok(ChatResponse {
                content: (!call).then(|| "broker round trip complete".into()), reasoning_content: None,
                tool_calls: if call { vec![ToolCall { id: "host-call".into(), name: "echo".into(), arguments: json!({"text":"fixture"}), metadata: None }] } else { vec![] },
                stop_reason: if call { StopReason::ToolUse } else { StopReason::EndTurn },
                usage: TokenUsage::default(), provider_index: None,
            })
        }
        fn model_id(&self) -> &str { "host-test" }
        fn provider_name(&self) -> &str { "host" }
        fn context_window(&self) -> u32 { 32768 }
        fn max_output_tokens(&self) -> u32 { 1024 }
    }

    struct Echo(&'static str, Arc<AtomicUsize>);
    impl crate::mcp::Tool for Echo {
        fn name(&self) -> &str { self.0 }
        fn description(&self) -> &str { "Echo test input" }
        fn input_schema(&self) -> Value { json!({"type":"object","properties":{"text":{"type":"string"}}}) }
        fn call(&self, _: &serde_json::Map<String, Value>) -> Result<String, String> {
            self.1.fetch_add(1, Ordering::AcqRel);
            Ok("host tool result".into())
        }
    }

    fn fixture(tool_loop: bool) -> (Arc<HostBroker>, Arc<Model>, Arc<AtomicUsize>) {
        let model = Arc::new(Model { calls: AtomicUsize::new(0), wait: AtomicBool::new(false), tool_loop, seen_tools: Mutex::new(vec![]) });
        let epoch = Arc::new(AtomicUsize::new(1));
        let captured = epoch.clone();
        let check: Check = Arc::new(move || if captured.load(Ordering::Acquire) == 1 { Ok(()) } else { Err("Stale activation.".into()) });
        let broker = HostBroker::with_provider(model.clone(), McpServer::new(), check, Arc::new(AtomicBool::new(false)), "Assistant".into()).unwrap();
        (broker, model, epoch)
    }

    pub(super) fn protocol_fixture() -> Arc<HostBroker> { fixture(false).0 }

    fn negotiate(broker: &HostBroker) {
        broker.accept_handshake(&json!({"agentCapabilities":{"_meta":{CAPABILITY_KEY:{"version":VERSION,"confined":true,"sandbox":"test"}}}})).unwrap();
    }

    fn request(broker: &HostBroker, id: u64, method: &str, params: Value) -> Result<std::sync::mpsc::Receiver<Result<Value, String>>, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let bytes = serde_json::to_vec(&params).unwrap().len();
        broker.submit(&json!(id), method, params, bytes, Box::new(move |result| { let _ = tx.send(result); }))?;
        Ok(rx)
    }

    fn model_request() -> Value { serde_json::to_value(ModelRequest { messages: vec![], tools: vec![], config: ChatConfig::default() }).unwrap() }

    #[test]
    fn broker_requires_negotiation_active_turn_and_captured_activation() {
        let (broker, model, epoch) = fixture(false);
        assert!(request(&broker, 1, MODEL_METHOD, model_request()).is_err());
        assert!(broker.accept_handshake(&json!({"agentCapabilities":{}})).is_err());
        negotiate(&broker);
        assert!(request(&broker, 1, MODEL_METHOD, model_request()).is_err());
        assert!(request(&broker, 1, TOOLS_LIST_METHOD, json!({})).unwrap().recv_timeout(Duration::from_secs(2)).unwrap().is_ok());
        broker.begin_turn().unwrap();
        let reply = request(&broker, 2, MODEL_METHOD, model_request()).unwrap().recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
        assert_eq!(reply["content"], "broker round trip complete");
        broker.cancel();
        assert!(request(&broker, 3, MODEL_METHOD, model_request()).is_err());
        assert!(request(&broker, 4, TOOLS_CALL_METHOD, json!({"name":"echo","arguments":{}})).is_err());
        broker.begin_turn().unwrap();
        epoch.store(2, Ordering::Release);
        assert!(request(&broker, 5, MODEL_METHOD, model_request()).is_err());
        assert_eq!(model.calls.load(Ordering::Acquire), 1);
        broker.stop();
    }

    #[test]
    fn cancellation_interrupts_model_wait_and_bounds_untrusted_work() {
        let (broker, model, _) = fixture(false);
        negotiate(&broker);
        broker.begin_turn().unwrap();
        model.wait.store(true, Ordering::Release);
        let mut replies = Vec::new();
        for id in 0..MAX_PENDING { replies.push(request(&broker, id as u64, MODEL_METHOD, model_request()).unwrap()); }
        assert!(request(&broker, 0, MODEL_METHOD, model_request()).is_err());
        assert!(request(&broker, 100, MODEL_METHOD, model_request()).is_err());
        let deadline = Instant::now() + Duration::from_secs(2);
        while model.calls.load(Ordering::Acquire) == 0 && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(5)); }
        assert!(model.calls.load(Ordering::Acquire) > 0);
        broker.cancel();
        for reply in replies { assert!(reply.recv_timeout(Duration::from_secs(2)).unwrap().is_err()); }
        assert!(model.calls.load(Ordering::Acquire) <= MAX_ACTIVE);
        model.wait.store(false, Ordering::Release);
        broker.begin_turn().unwrap();
        assert!(request(&broker, 101, MODEL_METHOD, model_request()).unwrap().recv_timeout(Duration::from_secs(2)).unwrap().is_ok());
        broker.stop();
    }

    #[test]
    fn tool_execution_uses_current_host_registry_and_rejects_forged_identity() {
        let (broker, _, _) = fixture(false);
        let calls = Arc::new(AtomicUsize::new(0));
        broker.tools.add_tool(Echo("echo", calls.clone()));
        negotiate(&broker);
        broker.begin_turn().unwrap();
        let call = json!({"name":"echo","arguments":{}});
        assert!(request(&broker, 1, TOOLS_CALL_METHOD, call.clone()).unwrap().recv_timeout(Duration::from_secs(2)).unwrap().is_ok());
        assert_eq!(calls.load(Ordering::Acquire), 1);
        broker.tools.remove_tool("echo");
        assert!(request(&broker, 2, TOOLS_CALL_METHOD, call).unwrap().recv_timeout(Duration::from_secs(2)).unwrap().is_err());
        assert!(request(&broker, 3, TOOLS_CALL_METHOD, json!({"name":"echo","arguments":{},"context":"other-room"})).unwrap().recv_timeout(Duration::from_secs(2)).unwrap().is_err());
        assert_eq!(calls.load(Ordering::Acquire), 1);
        broker.stop();
    }

    #[test]
    fn cancelled_blocking_tools_cannot_accumulate_detached_work() {
        struct Gate {
            entered: AtomicUsize,
            released: Mutex<bool>,
            changed: std::sync::Condvar,
        }
        struct BlockingTool(Arc<Gate>);
        impl crate::mcp::Tool for BlockingTool {
            fn name(&self) -> &str { "blocking" }
            fn description(&self) -> &str { "Wait for the test gate" }
            fn input_schema(&self) -> Value { json!({"type":"object"}) }
            fn call(&self, _: &serde_json::Map<String, Value>) -> Result<String, String> {
                self.0.entered.fetch_add(1, Ordering::AcqRel);
                let mut released = self.0.released.lock().unwrap();
                while !*released { released = self.0.changed.wait(released).unwrap(); }
                Ok("released".into())
            }
        }
        struct ReleaseOnDrop(Arc<Gate>);
        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                *self.0.released.lock().unwrap() = true;
                self.0.changed.notify_all();
            }
        }
        let (broker, _, _) = fixture(false);
        let gate = Arc::new(Gate { entered: AtomicUsize::new(0), released: Mutex::new(false), changed: std::sync::Condvar::new() });
        let _release = ReleaseOnDrop(gate.clone());
        broker.tools.add_tool(BlockingTool(gate.clone()));
        let tool = broker.tools.tools().into_iter().find(|tool| tool.name() == "blocking").unwrap();
        let retained_without_calls = Arc::strong_count(&tool);
        negotiate(&broker);
        broker.begin_turn().unwrap();
        let mut replies = Vec::new();
        for id in 0..MAX_ACTIVE {
            replies.push(request(&broker, id as u64, TOOLS_CALL_METHOD, json!({"name":"blocking","arguments":{}})).unwrap());
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while gate.entered.load(Ordering::Acquire) < MAX_ACTIVE && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(gate.entered.load(Ordering::Acquire), MAX_ACTIVE);
        broker.cancel();
        for reply in replies { assert!(reply.recv_timeout(Duration::from_secs(2)).unwrap().is_err()); }

        // Keep the original blocking calls alive while repeatedly admitting
        // and cancelling later turns. Queued blocking closures would retain
        // another tool Arc (and their full arguments) after each cancellation.
        for turn in 1..=4 {
            broker.begin_turn().unwrap();
            let mut replies = Vec::new();
            for index in 0..MAX_ACTIVE {
                replies.push(request(&broker, (turn * MAX_ACTIVE + index) as u64, TOOLS_CALL_METHOD,
                    json!({"name":"blocking","arguments":{"payload":"x".repeat(1024)}})).unwrap());
            }
            std::thread::sleep(Duration::from_millis(30));
            broker.cancel();
            for reply in replies { assert!(reply.recv_timeout(Duration::from_secs(2)).unwrap().is_err()); }
            let deadline = Instant::now() + Duration::from_secs(2);
            while Arc::strong_count(&tool) > retained_without_calls + MAX_ACTIVE && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(Arc::strong_count(&tool), retained_without_calls + MAX_ACTIVE,
                "cancelled calls retained extra blocking closures and arguments");
        }
        assert_eq!(gate.entered.load(Ordering::Acquire), MAX_ACTIVE);
        broker.stop();
    }

    #[test]
    #[ignore = "requires ROBRIX_TEST_OCTOS pointing to the built host-managed Octos executable"]
    fn confined_octos_uses_parent_model_tools_and_cancellation() {
        use crate::acp_client::AcpEvent;
        let executable = PathBuf::from(std::env::var_os("ROBRIX_TEST_OCTOS").expect("set ROBRIX_TEST_OCTOS"));
        let (broker, model, _) = fixture(true);
        let calls = Arc::new(AtomicUsize::new(0));
        broker.tools.add_tool(Echo("echo", calls.clone()));
        let mut client = AcpClient::spawn_protected(&executable, broker.clone()).unwrap();
        let wait = |client: &mut AcpClient, ready: bool| {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                for event in client.drain_events() {
                    match event {
                        AcpEvent::SessionReady if ready => return,
                        AcpEvent::TurnDone { text, .. } if !ready => { assert!(text.contains("broker round trip complete")); return; }
                        AcpEvent::Error(error) | AcpEvent::ProcessGone(error) => panic!("{error}"),
                        _ => {},
                    }
                }
                assert!(Instant::now() < deadline, "confined Octos did not finish");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        wait(&mut client, true);
        client.send_prompt("Use echo then finish.");
        wait(&mut client, false);
        assert_eq!(calls.load(Ordering::Acquire), 1);
        assert!(model.calls.load(Ordering::Acquire) >= 2);
        broker.tools.add_tool(Echo("new_tool", calls.clone()));
        client.send_prompt("Finish again.");
        wait(&mut client, false);
        assert!(model.seen_tools.lock().unwrap().last().unwrap().contains(&"new_tool".into()));
        model.wait.store(true, Ordering::Release);
        let before = model.calls.load(Ordering::Acquire);
        client.send_prompt("Wait for cancellation.");
        let deadline = Instant::now() + Duration::from_secs(5);
        while model.calls.load(Ordering::Acquire) == before && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(10)); }
        assert!(model.calls.load(Ordering::Acquire) > before);
        client.cancel();
        assert!(request(&broker, 999, MODEL_METHOD, model_request()).is_err());
        drop(client);
        assert!(broker.stopped.load(Ordering::Acquire));
    }
}
