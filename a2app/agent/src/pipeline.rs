//! The create-app generation pipeline: prompt → streamed reply → extract the
//! fenced Splash source → validate it against the real parser → repair loop →
//! a ready-to-install `MiniAppManifest`.
//!
//! Modeled on octos-one's card pipeline (single fenced block as the ENTIRE
//! reply, client-side extraction, lint → one-shot repair), but the validator
//! here is the actual Splash parser/evaluator running in a throwaway isolate
//! with a captured-error sink — not a regex lint.

use makepad_widgets::*;
use serde_json::Value;

use a2app_core::header::parse_app_header;
use a2app_core::manifest::{A2AppScope, MiniAppId, MiniAppManifest};

use crate::acp_client::{AcpEvent, PlanStep};
use crate::prefs::AgentPrefs;
use crate::AgentTransport;

/// How many repair turns a generation may take after the first attempt.
const MAX_REPAIRS: u32 = 2;

/// Symbol glyphs the app fonts are known to lack (they render as
/// tofu boxes). Generated apps must use emoji / words / × instead; seen in
/// the wild when models reach for ✕-style icons.
const TOFU_GLYPHS: &[char] = &['✕', '✗', '✘', '⤡', '⤢', '➜', '↻', '⟳'];

/// A generation with NO agent events for this long is declared stalled. LLM
/// turns legitimately take a while, so this is generous — it only catches an
/// agent that is alive but silent (hung provider, dead network) which would
/// otherwise leave the UI busy until the user hits Stop.
const STALL_SECS: u64 = 240;

/// The two kinds of output an agent streams, which the retained transcript
/// keeps apart with a heading — they arrive interleaved and read as gibberish
/// run together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamKind {
    Thinking,
    Writing,
}

impl StreamKind {
    fn heading(self) -> &'static str {
        match self {
            Self::Thinking => "💭 Thinking",
            Self::Writing => "✍️ Writing",
        }
    }
}

/// Appends agent output to `out`, headed whenever its kind differs from `last`.
fn append_output(out: &mut String, last: &mut Option<StreamKind>, kind: StreamKind, text: &str) {
    if *last != Some(kind) {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(kind.heading());
        out.push('\n');
        *last = Some(kind);
    }
    out.push_str(text);
}

/// Where the generation pipeline currently is; drives the create UI's status.
#[derive(Debug, Clone, PartialEq)]
pub enum GenPhase {
    /// Agent spawned, handshake in flight.
    Connecting,
    /// The prompt (or a repair prompt) is out; the reply is streaming in.
    /// `attempt` is 0 for the first try, 1.. for repairs.
    Generating { attempt: u32 },
    /// Nothing in flight (after success/failure the pipeline is dropped).
    Done,
}

/// The outcome the app layer acts on after feeding an event batch through.
pub enum GenOutcome {
    /// Still working; the status line may have changed.
    Working,
    /// Success. `refine_of` is set when this UPDATES an existing app (the
    /// manifest keeps that app's id) rather than installing a new one.
    Ready {
        manifest: Box<MiniAppManifest>,
        refine_of: Option<MiniAppId>,
    },
    /// Failure, with a short human-readable reason.
    Failed(String),
}

/// Whether this generation creates a new app or modifies an existing one.
enum GenMode {
    Create,
    /// Refine `base`: the prompt carries its current source, and the result
    /// keeps its id (name/icon/tint may change via the header).
    Refine { base: Box<MiniAppManifest> },
}

/// One in-flight "create app" (or "refine app") request.
pub struct Generation {
    client: Box<dyn AgentTransport>,
    /// The user's request, verbatim.
    request: String,
    mode: GenMode,
    /// The scope the built manifest gets (a refine keeps its base's).
    scope: A2AppScope,
    phase: GenPhase,
    /// Repair turns used so far.
    repairs: u32,
    /// Live status line for the UI.
    status: String,
    /// Ids already taken (so a fresh id can be made unique against the registry).
    taken_ids: Vec<MiniAppId>,
    /// When the last agent event arrived, for the stall watchdog.
    last_event: std::time::Instant,
    /// Whether prompts may omit the inline guide (persistent-guide active AND this
    /// generation's backend is one that carries the persistent copy — a
    /// foreign agent via ROBRIX_AGENT_CMD may ignore AGENTS.md, so the
    /// env override always inlines).
    slim_prompts: bool,
    /// The current turn's streamed reply text (the code being written),
    /// accumulated for the panel's live tail. Cleared per turn.
    stream: String,
    /// The current turn's streamed *thinking*, same idea. Kept apart from
    /// `stream` so it can never reach the fence extractor, and so the panel can
    /// show it only while there's no real output yet.
    thought: String,
    /// EVERYTHING this run did, in arrival order: its log lines (phase changes,
    /// tool calls, validation errors) and the agent's thinking and code, across
    /// every repair turn.
    ///
    /// Separate from `stream`/`thought` because those are working buffers: the
    /// fence extractor consumes `stream`, so it has to be cleared at each turn
    /// boundary. This never is. The console is the only record of what the
    /// agent did, and it used to show a rolling 700-byte window of the current
    /// turn, so everything before that — the reasoning, the code that failed to
    /// compile, the earlier attempts — was gone before anyone could read it.
    transcript: String,
    /// What the transcript last carried, so switching between thinking and code
    /// gets a heading instead of the two running together mid-word. `None`
    /// after a log line.
    transcript_kind: Option<StreamKind>,
    /// The current model call's output while the host streams it in, by kind.
    /// The agent's own copy of each kind replaces it.
    preview: Vec<(StreamKind, String)>,
    /// The plan the agent last published, so an update can be diffed against it
    /// and only the CHANGES logged (it republishes the whole list every time).
    plan: Vec<PlanStep>,
    /// Bumped whenever agent events arrive, so the UI can tell when to redraw.
    revision: u64,
    /// What updating Claude Code would show live, while that is still being checked.
    update: Option<std::sync::mpsc::Receiver<crate::claude_code::Update>>,
}

impl Generation {
    /// Starts the agent backend and kicks off a generation for `request`.
    /// `taken_ids` is the current set of installed app ids; `scope` is what
    /// the built manifest gets (account-wide, or attached to one room).
    pub fn start(
        request: String,
        taken_ids: Vec<MiniAppId>,
        scope: A2AppScope,
        prefs: AgentPrefs,
        model_context: Option<a2app_core::information_flow::ContextId>,
    ) -> Result<Self, String> {
        Self::start_with_mode(request, taken_ids, scope, GenMode::Create, prefs, model_context)
    }

    /// Starts a refine of an existing app: same machinery, but the prompt
    /// includes the app's current source and the result keeps its id.
    pub fn start_refine(
        request: String,
        base: MiniAppManifest,
        prefs: AgentPrefs,
        model_context: Option<a2app_core::information_flow::ContextId>,
    ) -> Result<Self, String> {
        let scope = base.scope.clone();
        let mode = GenMode::Refine { base: Box::new(base) };
        Self::start_with_mode(request, Vec::new(), scope, mode, prefs, model_context)
    }

    fn start_with_mode(
        request: String,
        taken_ids: Vec<MiniAppId>,
        scope: A2AppScope,
        mode: GenMode,
        prefs: AgentPrefs,
        model_context: Option<a2app_core::information_flow::ContextId>,
    ) -> Result<Self, String> {
        let workspace = agent_workspace_dir();
        // Persist the dialect guide on the agent side so prompts can go slim.
        #[cfg(feature = "persistent-guide")]
        crate::skills::deploy_guide(&workspace);
        let client = crate::start_backend_with_mcp(&workspace, &prefs, &[], false, None, model_context, None)?;
        // Slim prompts only for backends known to carry the persistent guide:
        // the default octos spawn / the in-process agent. An explicit
        // ROBRIX_AGENT_CMD may be any ACP agent, which likely ignores
        // our AGENTS.md — inline the guide for those.
        #[cfg(feature = "persistent-guide")]
        let slim_prompts = std::env::var("ROBRIX_AGENT_CMD").is_err()
            && crate::skills::guide_is_deployed();
        #[cfg(not(feature = "persistent-guide"))]
        let slim_prompts = false;
        let update = check_update(crate::providers::effective_provider().as_deref(), &prefs);
        Ok(Self {
            client,
            request,
            mode,
            scope,
            phase: GenPhase::Connecting,
            repairs: 0,
            status: "Connecting to the agent…".to_string(),
            taken_ids,
            last_event: std::time::Instant::now(),
            thought: String::new(),
            plan: Vec::new(),
            slim_prompts,
            stream: String::new(),
            transcript: String::new(),
            transcript_kind: None,
            preview: Vec::new(),
            revision: 0,
            update,
        })
    }

    /// The user's request, verbatim — recorded as the version-history note
    /// when a modification lands.
    pub fn request(&self) -> &str {
        &self.request
    }

    /// The app this run modifies, or `None` when it creates a new one. The
    /// UI's Retry needs it to re-run the same kind of generation.
    pub fn refine_target(&self) -> Option<&MiniAppId> {
        match &self.mode {
            GenMode::Create => None,
            GenMode::Refine { base } => Some(&base.id),
        }
    }

    /// Everything the run did so far, oldest first, ending with any output
    /// still streaming in. What the console shows.
    pub fn console(&self) -> String {
        let (mut console, mut kind) = (self.transcript.clone(), self.transcript_kind);
        for (previewed, text) in &self.preview {
            append_output(&mut console, &mut kind, *previewed, text);
        }
        console
    }

    /// What the create UI shows while a generation is live.
    ///
    /// No run clock: a ticking `m:ss` next to the status invited the user to
    /// watch a number instead of the work, and said nothing about progress —
    /// the spinner already carries "still going", and the status text itself
    /// changes as the run moves through its phases.
    pub fn status_line(&self) -> String {
        self.status.clone()
    }

    /// Changes whenever the status, trail or transcript may have changed.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Adds a line to the transcript where it happened, so the console reads
    /// in time order. Nothing is ever dropped: a pipeline lives only as long
    /// as the generation it drives, so the transcript is bounded by the run.
    fn log(&mut self, line: impl Into<String>) {
        if !self.transcript.is_empty() {
            // A blank line sets the log apart from the agent's own output.
            self.transcript.push_str(if self.transcript_kind.is_some() { "\n\n" } else { "\n" });
        }
        self.transcript.push_str(&line.into());
        self.transcript_kind = None;
    }

    /// Appends agent output to the retained transcript, heading it whenever the
    /// kind changes so a stretch of reasoning doesn't run straight into code.
    fn transcribe(&mut self, kind: StreamKind, text: &str) {
        // The copy replaces the preview of its kind. A reply's code follows its
        // thinking, so copied code replaces the whole preview.
        match kind {
            StreamKind::Thinking => self.preview.retain(|(previewed, _)| *previewed != kind),
            StreamKind::Writing => self.preview.clear(),
        }
        append_output(&mut self.transcript, &mut self.transcript_kind, kind, text);
    }

    /// Shows streamed output after the transcript, headed the same way, until the
    /// agent's own copy arrives through `transcribe`.
    fn show_preview(&mut self, kind: StreamKind, text: &str) {
        match self.preview.last_mut() {
            Some((previewed, shown)) if *previewed == kind => shown.push_str(text),
            _ => self.preview.push((kind, text.to_string())),
        }
    }

    /// Whether the app's code shows yet, copied or still streaming in.
    fn is_writing(&self) -> bool {
        !self.stream.is_empty() || self.preview.last().is_some_and(|(previewed, _)| *previewed == StreamKind::Writing)
    }

    /// Drops output the agent has not confirmed, as when its run ends early.
    pub fn clear_preview(&mut self) {
        self.preview.clear();
    }

    /// The status while the agent writes (`kind`), named for the current attempt.
    fn writing_status(&mut self, kind: StreamKind) {
        let GenPhase::Generating { attempt } = self.phase else { return };
        self.status = match (kind, attempt) {
            (StreamKind::Writing, 0) => "Writing the app…".to_string(),
            (StreamKind::Writing, _) => format!("Fixing the app (try {attempt})…"),
            (StreamKind::Thinking, 0) => "Thinking…".to_string(),
            (StreamKind::Thinking, _) => format!("Thinking about the fix (try {attempt})…"),
        };
    }

    /// Marks a turn boundary in the transcript. Each repair re-asks the agent
    /// from scratch, and without this the new attempt's code runs straight on
    /// from the failed one's with nothing to say which is which.
    fn transcribe_turn(&mut self, heading: &str) {
        if !self.transcript.is_empty() {
            self.transcript.push_str("\n\n");
        }
        self.transcript.push_str(heading);
        // Force a heading on the next chunk whatever kind it is.
        self.transcript_kind = None;
    }

    /// The current status line for the create UI.
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Asks the agent to abandon the in-flight turn. The caller drops the
    /// pipeline right after; this just lets the agent stop burning tokens.
    pub fn cancel(&mut self) {
        self.client.cancel();
    }

    /// Whether the agent has gone silent past the stall budget. Checked from a
    /// periodic timer, since a silent agent by definition produces no events
    /// that would otherwise wake the pipeline.
    pub fn is_stalled(&self) -> bool {
        self.phase != GenPhase::Done && self.last_event.elapsed().as_secs() > STALL_SECS
    }

    /// Logs what updating Claude Code would show live, once its check finishes.
    fn show_update(&mut self) {
        let Some(update) = &self.update else { return };
        match update.try_recv() {
            Ok(update) => {
                self.log(update_notice(update));
                self.revision += 1;
                self.update = None;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.update = None,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    /// Feeds queued agent events through the state machine. Call on every UI
    /// event (cheap when idle); `cx` is needed to run the validator.
    pub fn advance(&mut self, cx: &mut Cx) -> GenOutcome {
        self.show_update();
        let events = self.client.drain_events();
        if !events.is_empty() {
            self.last_event = std::time::Instant::now();
            self.revision += 1;
        }
        for event in events {
            match event {
                AcpEvent::SessionReady => {
                    let prompt = match &self.mode {
                        GenMode::Create => build_initial_prompt(&self.request, self.slim_prompts),
                        GenMode::Refine { base } => {
                            build_refine_prompt(&self.request, base, self.slim_prompts)
                        }
                    };
                    self.client.send_prompt(&prompt);
                    self.phase = GenPhase::Generating { attempt: 0 };
                    // Until the model's output starts streaming in.
                    self.status = "Waiting for the model…".to_string();
                    self.log("Agent connected");
                    self.stream.clear();
                    self.thought.clear();
                    self.transcript_kind = None;
                }
                AcpEvent::Chunk(text) => {
                    if self.stream.is_empty() {
                        makepad_widgets::log!("app generation: agent writing…");
                    }
                    self.stream.push_str(&text);
                    self.transcribe(StreamKind::Writing, &text);
                    self.writing_status(StreamKind::Writing);
                }
                AcpEvent::Preview { is_thought, text } => {
                    let kind = if is_thought { StreamKind::Thinking } else { StreamKind::Writing };
                    let was_writing = self.is_writing();
                    self.show_preview(kind, &text);
                    if kind == StreamKind::Writing || !was_writing {
                        self.writing_status(kind);
                    }
                }
                AcpEvent::ToolCall { title, .. } => {
                    makepad_widgets::log!("app generation: agent tool call started");
                    self.clear_preview();
                    self.status = format!("Agent: {title}…");
                    self.log(format!("🔧 {title}"));
                }
                AcpEvent::ToolCallDone { ok, .. } => {
                    // The console's per-call line is enough; the completion is
                    // surfaced only so the stall clock moved (see `advance`).
                    makepad_widgets::log!("app generation: agent tool call finished (ok: {ok})");
                }
                AcpEvent::Thought(text) => {
                    // The transcript's heading marks the stretch; the console log
                    // gets one line per stretch, not one per token-sized chunk.
                    if self.thought.is_empty() {
                        makepad_widgets::log!("app generation: agent thinking…");
                    }
                    self.thought.push_str(&text);
                    self.transcribe(StreamKind::Thinking, &text);
                    // The copied thinking can come a pass before the code it preceded.
                    if !self.is_writing() {
                        self.writing_status(StreamKind::Thinking);
                    }
                }
                AcpEvent::Plan(steps) => {
                    // The agent republishes the whole plan on every change, so
                    // log the diff: new steps once, and each step again when it
                    // starts or finishes. Otherwise a five-step plan floods the
                    // console with five copies of itself per update.
                    for step in &steps {
                        let was = self.plan.iter().find(|p| p.content == step.content);
                        if was.map(|p| &p.status) == Some(&step.status) {
                            continue;
                        }
                        match step.status.as_str() {
                            "completed" => self.log(format!("✓ {}", step.content)),
                            "in_progress" => {
                                self.status = format!("{}…", step.content);
                                self.log(format!("→ {}", step.content));
                            }
                            _ if was.is_none() => self.log(format!("📋 {}", step.content)),
                            _ => {}
                        }
                    }
                    self.plan = steps;
                }
                // No content, but it moved the stall clock (see `advance`).
                AcpEvent::Tick => {}
                AcpEvent::TurnDone { stop_reason, text } => {
                    makepad_widgets::log!("app generation: turn done ({stop_reason})");
                    self.clear_preview();
                    if stop_reason == "cancelled" {
                        return GenOutcome::Failed("Cancelled".to_string());
                    }
                    if stop_reason == "refusal" {
                        return GenOutcome::Failed("The agent declined that request".to_string());
                    }
                    match self.finish_turn(cx, &text) {
                        TurnVerdict::Installed(manifest) => {
                            makepad_widgets::log!(
                                "app generation: validated clean (attempt {})",
                                self.repairs
                            );
                            self.phase = GenPhase::Done;
                            let refine_of = match &self.mode {
                                GenMode::Create => None,
                                GenMode::Refine { base } => Some(base.id.clone()),
                            };
                            return GenOutcome::Ready { manifest, refine_of };
                        }
                        TurnVerdict::NeedsRepair { errors, colors } => {
                            self.repairs += 1;
                            let problems = errors.len() + colors.len();
                            // Shown even for the last attempt, so the console says why the run failed.
                            for e in errors.iter().chain(&colors).take(3) {
                                self.log(format!("⚠ {}", display_error(e)));
                            }
                            if problems > 3 {
                                self.log(format!("…and {} more", problems - 3));
                            }
                            if self.repairs > MAX_REPAIRS {
                                let reason = if errors.is_empty() {
                                    "The generated app kept using colors that don't follow the dark theme"
                                } else {
                                    "The generated app kept failing to compile"
                                };
                                return GenOutcome::Failed(reason.to_string());
                            }
                            makepad_widgets::log!(
                                "app generation: {} problem(s) found — repair {} (of max {})",
                                problems,
                                self.repairs,
                                MAX_REPAIRS
                            );
                            let attempt = self.repairs;
                            self.client.send_prompt(&build_repair_prompt(&errors, &colors));
                            self.phase = GenPhase::Generating { attempt };
                            self.status = format!("Fixing the app (try {attempt})…");
                            self.stream.clear();
                            self.thought.clear();
                            let count = match problems {
                                1 => "1 error".to_string(),
                                count => format!("{count} errors"),
                            };
                            self.transcribe_turn(&format!("── Repair {attempt}: {count} sent back ──"));
                        }
                        TurnVerdict::Malformed(reason) => {
                            // No fenced code at all — one retry with a nudge,
                            // sharing the repair budget so this can't loop.
                            self.repairs += 1;
                            if self.repairs > MAX_REPAIRS {
                                return GenOutcome::Failed(reason);
                            }
                            makepad_widgets::log!(
                                "app generation: reply had no fenced block — nudging (attempt {})",
                                self.repairs
                            );
                            let attempt = self.repairs;
                            self.client.send_prompt(&build_nudge_prompt());
                            self.phase = GenPhase::Generating { attempt };
                            self.status = "Asking for the code again…".to_string();
                            self.stream.clear();
                            self.thought.clear();
                            self.transcribe_turn("── Retry: the reply had no code block ──");
                        }
                    }
                }
                AcpEvent::Error(msg) => {
                    // A failed *spawn* arrives here, not as ProcessGone — the
                    // process never existed to go away — so it needs the same
                    // "install octos" translation rather than a raw errno.
                    self.clear_preview();
                    return GenOutcome::Failed(process_gone_reason(&msg, self.client.desc()));
                }
                AcpEvent::ProcessGone(msg) => {
                    self.clear_preview();
                    return GenOutcome::Failed(process_gone_reason(&msg, self.client.desc()));
                }
            }
        }
        GenOutcome::Working
    }

    /// Extract + validate a finished turn. On success, returns the manifest.
    fn finish_turn(&mut self, cx: &mut Cx, reply: &str) -> TurnVerdict {
        let Some(block) = extract_splash_block(reply) else {
            return TurnVerdict::Malformed(
                "The agent's reply had no ```splash code block".to_string(),
            );
        };
        // The header comments stay in the source, so `block` IS the source.
        let header = parse_app_header(&block);
        let source = block;

        if let Some(banned) = forbidden_construct(&source) {
            let error = format!(
                "the script uses `{banned}`, which is not available in this dialect \
                 — remove it and use only constructs from the guide"
            );
            return TurnVerdict::NeedsRepair { errors: vec![error], colors: Vec::new() };
        }
        // Runtime-invisible but user-visible: glyphs the app fonts don't have
        // render as tofu boxes. Caught here (the parser can't) so the repair
        // turn swaps them out.
        if let Some(c) = source.chars().find(|c| TOFU_GLYPHS.contains(c)) {
            let error = format!(
                "the app font has no glyph for '{c}' (renders as an empty box) — \
                 use an emoji, a plain word, or × (U+00D7) instead"
            );
            return TurnVerdict::NeedsRepair { errors: vec![error], colors: Vec::new() };
        }

        self.status = "Checking the app…".to_string();
        self.log("Checking the app…");
        let errors = validate_splash(cx, &source);
        // Fixed colors compile, but stay light-tuned on the dark theme.
        let colors = bare_color_errors(&source);
        if !errors.is_empty() || !colors.is_empty() {
            return TurnVerdict::NeedsRepair { errors, colors };
        }
        self.log("No errors found");

        // A refine keeps the app's identity; the header may restyle it. A
        // create mints everything fresh.
        let manifest = match &self.mode {
            GenMode::Create => {
                let name = header.name.unwrap_or_else(|| default_name(&self.request));
                let id = unique_id(&name, &self.taken_ids);
                MiniAppManifest {
                    id,
                    name,
                    icon: header.icon.unwrap_or_else(|| "✨".to_string()),
                    tint: header.tint.unwrap_or(0x7c6cf0),
                    description: header.description.clone().unwrap_or_default(),
                    source,
                    allow_net: false,
                    // Generated apps start fully sandboxed; the user (or a
                    // later refine) adds declarations deliberately.
                    // What the agent asked for, sanitized. Declaring is not
                    // granting: runtime tiers prompt on first use and App Info
                    // lists every one of them.
                    permissions: header.permissions.clone(),
                    permission_reasons: header.permission_reasons.clone(),
                    capabilities: header.capabilities.clone(),
                    builtin: false,
                    widget: None,
                    shortcuts: Vec::new(),
                    scope: self.scope.clone(),
                    current_version: None,
                }
            }
            GenMode::Refine { base } => a2app_core::manifest::rewritten(base, source),
        };
        TurnVerdict::Installed(Box::new(manifest))
    }
}

enum TurnVerdict {
    Installed(Box<MiniAppManifest>),
    /// What to send back: compile or construct `errors`, and fixed `colors`.
    NeedsRepair { errors: Vec<String>, colors: Vec<String> },
    Malformed(String),
}

/// The harmless directory the agent's tools are rooted at (octos insists on a
/// cwd for its sandbox), under the a2app data root.
pub fn agent_workspace_dir() -> std::path::PathBuf {
    a2app_core::data_root().join("agent_workspace")
}

/// On Claude Code, asks off the UI thread whether updating it would show more
/// of a run live, waking the UI if so. Switched-off thinking can't show anyway.
fn check_update(
    provider: Option<&str>,
    prefs: &AgentPrefs,
) -> Option<std::sync::mpsc::Receiver<crate::claude_code::Update>> {
    if provider != Some(crate::claude_code::ID) {
        return None;
    }
    let is_thinking = prefs.thinking.as_deref() != Some("off");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let update = crate::claude_code::preview_update()
            .filter(|update| is_thinking || matches!(update, crate::claude_code::Update::Streaming(_)));
        if let Some(update) = update {
            let _ = sender.send(update);
            SignalToUI::set_ui_signal();
        }
    });
    Some(receiver)
}

/// The console's suggestion to update Claude Code, naming the installed version.
fn update_notice(update: crate::claude_code::Update) -> String {
    use crate::claude_code::Update;
    let named = |version: Option<(u32, u32, u32)>| match version {
        Some((major, minor, patch)) => format!("Claude Code {major}.{minor}.{patch}"),
        None => "This version of Claude Code".to_string(),
    };
    match update {
        Update::Streaming(version) => format!("{} can't show the model's output live. \
            Run `claude update` in a terminal to see it as it's written.", named(version)),
        Update::Thinking(version) => format!("{} shows the app as it's written, but not newer models' thinking. \
            Run `claude update` in a terminal to see that too.", named(Some(version))),
    }
}

// ---------------------------------------------------------------------------
// Prompts
// ---------------------------------------------------------------------------

use crate::SPLASH_GUIDE;

/// With `research`, the agent may research before answering (web search /
/// fetch — octos's own tools), baking what it finds into the app as
/// constants. Without it, generation is a single pure text turn.
#[cfg(feature = "research")]
const TOOL_POLICY: &str = "You MAY use your tools first (e.g. web search/fetch) to look up real \
     data the app should carry — exchange rates, schedules, trivia — and bake \
     the results into the script as plain constants (the app itself has no \
     network access). Your FINAL message must still be exactly the one fenced \
     block, nothing else.";
#[cfg(not(feature = "research"))]
const TOOL_POLICY: &str = "Do not use tools; reply directly.";

/// The reply contract shared by create, refine, and repair turns.
fn reply_contract() -> String {
    format!(
        "## Reply format (MANDATORY)\n\
         \n\
         Reply with EXACTLY ONE fenced code block and nothing else — no prose \
         before or after, no extra fences. {TOOL_POLICY}\n\
         \n\
         ```splash\n\
         // name: <Short App Name, max 18 chars>\n\
         // icon: <one emoji>\n\
         // tint: <hex color like #4A90D9 that suits the app>\n\
         <the complete script>\n\
         ```"
    )
}

/// The dialect guide, unless this generation's backend carries a persistent
/// copy (`persistent-guide` deploys one — see `crate::skills`), in which case a
/// one-line pointer replaces the ~6KB text.
fn guide_section(slim: bool) -> String {
    if slim {
        return "Follow the Splash dialect guide in your workspace's AGENTS.md \
                (also in your system prompt) EXACTLY — only constructs shown \
                there exist."
            .to_string();
    }
    SPLASH_GUIDE.to_string()
}

/// Stated in the task itself, not only the guide: the host shows apps
/// fullscreen, in split-screen panes, and in freely resized windows, and an
/// app that only works phone-shaped is a broken deliverable.
const RESPONSIVE_POLICY: &str = "The app MUST lay out well and stay fully usable at ANY host size — \
     fullscreen phone, a narrow (~190pt) or short (~250pt) split-screen pane, \
     or a wide desktop window. Fill the available pane: root and content column \
     use `width: Fill height: Fill`, controls use `height: Fit`, and primary \
     scrolling lists use `height: Fill{min: 80}` for the remaining space. Make \
     the content column itself a ScrollYView so controls remain reachable in \
     short panes. Never cap the whole content column's width or use fixed \
     result-list heights. Keep tappable dynamic lists as ScrollYView so item \
     dragging scrolls correctly. When \
     fixed sizes or fonts must differ by size, define `fn on_app_resize(w, h)` \
     and toggle pre-declared tier Views (see the guide's \"runs at ANY size\" \
     section).";

/// Stated in EVERY prompt: Robrix switches between light and dark live by re-running
/// the app, and colors it writes itself can't follow.
const THEME_POLICY: &str = "Robrix has a light and a dark theme, and the user can switch \
     while the app runs. Take EVERY color from `theme.palette.<name>` (see the guide's \
     Robrix look); write a hex color only in the `// tint:` header, or inside \
     `if theme.palette.is_dark {...} else {...}` for a color with no palette role. Inside \
     a shader fn (`pixel:`, `get_color:`, ...) never read `theme.palette` or write a \
     color, since shaders keep the colors of the theme they were first built in: declare \
     it on the draw object (`mark_color: instance(theme.palette.accent_text)`) and use \
     `self.mark_color`. A theme switch runs the script again from the top, so keep data \
     that must survive in `mod.state` or the `fs` jail, and make the top level safe to \
     repeat: its requests only read or set up (permissions, subscriptions), never send, \
     post or join. Mark an action in flight in `mod.state`; after a re-run, reset its \
     \"…ing\" status and treat the action as possibly done, never resending it (the \
     guide shows how).";

/// Refining an app that still writes fixed colors: they become the palette colors
/// with the same light values, so it keeps its look and gains dark mode.
const HEX_TO_PALETTE: &str = "This app's code still writes fixed colors, which look the \
     same on the dark theme. As part of this change, replace EVERY one (any case, `#fff` \
     = `#xFFFFFF`; a `vec4` of values from 0 to 1 is a color too) with its \
     `theme.palette` color, which keeps the light look: #x1C274C title, #x333333 body, \
     #x66708F muted, #xFFFFFF on_accent (text or icons on a colored fill) or surface \
     (backgrounds), #xFFFFFF00 transparent, #x0F88FE accent (fills, hover borders) or \
     accent_text (text, icons, spinners), #x106FCC accent_hover (fills) or focus \
     (field borders), #x0C5DAA accent_down, #xE3E3E3 neutral, #xD0D0D0 neutral_hover, \
     #xC0C0C0 neutral_down, #xE0E0E0 neutral_border (button borders) or card_border \
     (outlines of cards and framed boxes), #xC8C8C8 border, #xF0F5FF card, \
     #xE4EEFF pressed, #xF7F9FC card_subtle, #x138808 success, #xF0FFF0 success_bg, \
     #xD4EED4 success_bg_hover, #xB8E0B8 success_bg_down, #xDC0005 danger, #xFFF0F0 \
     danger_bg, #xF0D4D4 danger_bg_hover, #xE0B8B8 danger_bg_down, #xBBBBBB \
     placeholder, #x999999 placeholder_hover, #x52B2AC avatar, #x00000018 divider. \
     Any other color, and a #x138808 or #xDC0005 FILL behind text (success and danger \
     turn light in the dark theme), becomes `if theme.palette.is_dark {<a value that \
     suits the #262626 dark pane and any text on it>} else {<its old value>}`. A color \
     inside a shader fn (`pixel:`, `get_color:`, ...) moves onto the draw object, as \
     `mark_color: instance(theme.palette.title)` used as `self.mark_color`, since shaders \
     keep their first theme's colors. Leave no fixed color behind: one left over looks \
     the same on the dark theme.";

/// The conversion a refine asks for, if `source` still writes fixed colors.
fn theme_conversion(source: &str) -> String {
    if a2app_core::palette::bare_colors(source).is_empty() {
        return String::new();
    }
    format!("{HEX_TO_PALETTE}\n\n")
}

/// Each fixed color `source` writes, as a line for the repair turn.
fn bare_color_errors(source: &str) -> Vec<String> {
    a2app_core::palette::bare_colors(source).into_iter().map(|color| match color.light_of.as_slice() {
        [] => format!("Line {}: `{}` won't follow the dark theme", color.line, color.text),
        names => format!(
            "Line {}: `{}` won't follow the dark theme; it is the light value of `theme.palette.{}`",
            color.line, color.text, names.join("` or `theme.palette."),
        ),
    }).collect()
}

/// Stated in EVERY prompt, slim or not: an agent that never sees the full
/// guide still has to get consent right, and "works only if granted" is the
/// most likely way a generated app is broken on arrival.
const PERMISSION_POLICY: &str = "The app is SANDBOXED. Anything beyond its own UI and \
     its private `fs` jail (network, location, clipboard, notifications, links, files, \
     share, auth, messaging other apps, Matrix room data and messages, the user's \
     Matrix identity, steering Robrix to a room/thread/message/profile, drafting into \
     its composer) needs a capability DECLARED in the header \
     (`// permissions: network, location`) with a reason per capability \
     (`// why-network: ...`). Declaring is not granting: the user is asked, and can \
     refuse or revoke at any time. So the app MUST be fully usable with everything \
     denied — real fallback content, never a blank screen or a dead button — must \
     handle `r.is_ok == false` in every `host.request` callback, must re-check \
     `host.has(\"x\")` at use time rather than caching it at boot, and should define \
     `fn on_permissions_changed(caps)` to re-sync anything gated, reloading only \
     allowed abilities that have not already completed and never retrying a denial \
     from that hook. For startup needing several read/watch groups, use one \
     `permissions.request` with `{perms:[\"group\",...]}` for only those declared \
     groups; honor partial `r.data.permissions` selections and exact `host.has` \
     checks. Use `{retry:true}` only from a visible Refresh/Test click after denial. \
     Single Fetch/Post/Open actions call their concrete service directly to keep \
     one-time approval; never batch unspecified network access or unused writes. \
     Draft/Prepare/Attach controls use `composer.insert` or `composer.attach`: \
     declare only `host.composer.insert`/`host.composer.attach` for local composer \
     changes, independently of Matrix message-send or media-upload permissions. \
     `composer.attach` takes raw `data_base64` (4 MiB encoded max), one `filename`, \
     concrete `mime_type`, and optional `caption`/`room_id`; it opens a local \
     attachment preview and never posts. `files.pick` supplies lossless \
     `data_base64`/`mime_type` plus `name`, `size`, and `text` for selected files \
     up to 1 MiB; handle cancellation and oversize errors. A Send/Post control \
     may call `matrix.send_message` for text or `matrix.send_media` for native \
     media only when the user explicitly asks to post. `matrix.send_media` \
     targets only the attached room and accepts the same inline media fields \
     as composer.attach without room_id; declare `matrix.media.upload` and \
     `matrix.media.send` and handle each grant independently from composer \
     grants. Label buttons for their \
     actual effect and never replace a refused draft with a direct send. \
     Ask for the least it needs. The host also limits HOW OFTEN an app may ask: requests are charged \
     against a per-app budget, so ask on user action or when data goes stale — never \
     poll in a loop, never request from a fast timer, never retry a failure \
     immediately. Over-budget requests come back as `r.is_ok == false` (handle them \
     like a denial), and an app that keeps hammering is STOPPED by the host. \
     File pickers, save dialogs and `auth.check` work only while the app is on \
     screen, one at a time. Apps also \
     SHARE the processor, memory, timers and downloads: an app on its own is not \
     limited, and is only trimmed when others compete for the same thing. Still \
     write for the edges — `start_interval` returns nil if refused (check it if \
     you create timers in a loop), an interval faster than the floor is SLOWED \
     rather than refused, and nothing should accumulate without bound.\n\
     \n\
     The `matrix.*` services (see the guide's Matrix services section) work only \
     when the app is attached to a room, and fail like a denial without one. Send \
     ONLY what the user explicitly asked to send, one message per user action, \
     never on a timer. Anything the app lists that maps onto Robrix (a member, a \
     thread, a message, a room) should be tappable via `on_item_tap` on the list and \
     the `nav.*` services (see the guide's Acting inside Robrix section), only ever \
     in response to a tap.";

/// The pace policy for the app generator. The pipeline wants ONE valid Splash
/// file, fast: a reasoning model may still stream its own thinking (nothing
/// Robrix sends can switch a model's default-on reasoning off), so the prompt
/// says outright what the pipeline needs — don't overthink, don't plan aloud,
/// write the first complete app that satisfies the request, in one reply.
pub(crate) const PACE_POLICY: &str = "Do NOT overthink this. Do not reason at \
     length and do not plan aloud — read the request once, then write the \
     complete, valid Splash app that meets it as a single fenced block, as \
     quickly as you can. A working app delivered now is better than a perfect \
     one; it can be refined later if the user asks.";

fn build_initial_prompt(request: &str, slim: bool) -> String {
    format!(
        "You are the app generator for a phone launcher. Build a small, polished, \
         self-contained mini-app in the Makepad Splash dialect described below. \
         {PACE_POLICY}\n\
         \n\
         {RESPONSIVE_POLICY}\n\
         \n\
         {THEME_POLICY}\n\
         \n\
         {PERMISSION_POLICY}\n\
         \n\
         {}\n\
         \n\
         {}\n\
         \n\
         User request: {request}",
        guide_section(slim),
        reply_contract(),
    )
}

fn build_refine_prompt(request: &str, base: &MiniAppManifest, slim: bool) -> String {
    format!(
        "You are the app generator for a phone launcher. MODIFY an existing \
         mini-app written in the Makepad Splash dialect described below. \
         {PACE_POLICY}\n\
         \n\
         Keep everything the user didn't ask to change — including its responsive \
         layout: {RESPONSIVE_POLICY}\n\
         \n\
         {THEME_POLICY}\n\
         \n\
         {PERMISSION_POLICY}\n\
         \n\
         {}\n\
         \n\
         ## The app's current source ({})\n\
         \n\
         ````splash\n{}\n````\n\
         \n\
         {}{}\n\
         \n\
         User's change request: {request}",
        guide_section(slim),
        base.name,
        base.source,
        theme_conversion(&base.source),
        reply_contract(),
    )
}

/// The repair turn for `errors` (compile and construct problems) and fixed `colors`.
/// It says "failed to compile" only when there are errors: the fake agent keys on it.
fn build_repair_prompt(errors: &[String], colors: &[String]) -> String {
    let list = |items: &[String]| items.iter().map(|item| format!("- {item}\n")).collect::<String>();
    let mut problems = String::new();
    if !errors.is_empty() {
        problems.push_str(&format!("Your script failed to compile. Errors:\n{}\n", list(errors)));
    }
    if !colors.is_empty() {
        problems.push_str(&format!(
            "Your script writes fixed colors, which look the same on the dark theme:\n{}\
             Take each from `theme.palette`, or pick it per theme with \
             `if theme.palette.is_dark {{...}} else {{...}}`; inside a shader fn, declare it \
             on the draw object as an `instance(...)` and use it through `self`.\n\n",
            list(colors),
        ));
    }
    format!(
        "{problems}Re-read the guide's rules and reply again with the corrected COMPLETE \
         script as EXACTLY ONE ```splash fenced block (same // name/icon/tint \
         header), nothing else. Fix it directly — do not overthink or re-plan; \
         correct what the errors point at and resend the whole script."
    )
}

fn build_nudge_prompt() -> String {
    "Your reply did not contain a ```splash fenced code block. Reply with \
     EXACTLY ONE ```splash fenced block containing the complete script \
     (starting with the // name / // icon / // tint header lines), and \
     nothing else."
        .to_string()
}

// ---------------------------------------------------------------------------
// Extraction
// ---------------------------------------------------------------------------

/// Pulls the body of the ```splash fence out of the reply. The agent is asked
/// for exactly one, but replies sometimes grow prose that *mentions* the fence
/// (before or after the real one) — so every tagged fence is scanned and the
/// longest body wins, which is robust against both a quoted-format preamble
/// and a trailing "as requested, one ```splash block" remark. Falls back to a
/// bare ``` fence if no tagged one exists. CRLF endings are normalized.
fn extract_splash_block(reply: &str) -> Option<String> {
    fn body_after(reply: &str, open_idx: usize) -> Option<&str> {
        let after = &reply[open_idx..];
        let body_start = after.find('\n')? + 1;
        let body = &after[body_start..];
        let end = body.find("```").unwrap_or(body.len());
        Some(body[..end].trim_end())
    }

    let mut best: Option<&str> = None;
    let mut from = 0;
    while let Some(rel) = reply[from..].find("```splash") {
        let idx = from + rel;
        if let Some(body) = body_after(reply, idx) {
            if best.is_none_or(|b| body.len() > b.len()) {
                best = Some(body);
            }
        }
        from = idx + "```splash".len();
    }
    let body = match best {
        Some(b) => b,
        // Fallback: a single untagged fence.
        None => body_after(reply, reply.find("```")?)?,
    };
    if body.trim().is_empty() {
        return None;
    }
    Some(body.replace("\r\n", "\n"))
}

/// Cheap pre-parse ban list: constructs from OTHER dialects (octos-one's
/// `sys.*` helpers, web substrates, raw module imports) that would either
/// parse-fail confusingly or signal the agent drifted off-dialect — caught
/// here with a clearer message than the parser would give. This is a
/// steering aid, NOT the sandbox: actual containment is the isolate itself
/// (no net runtime, and `fs`/`run`/`res` stripped from its namespace).
/// String literals are blanked before scanning so an app whose *label text*
/// mentions e.g. "import " isn't rejected.
fn forbidden_construct(source: &str) -> Option<&'static str> {
    const BANNED: &[&str] = &[
        "sys.", "live_design", "Root{", "Root {", "Window{", "Window {",
        "use mod", "import ", "<script", "<div",
    ];
    let bare = blank_string_literals(source);
    BANNED.iter().copied().find(|b| bare.contains(b))
}

/// Replaces the contents of double-quoted string literals with spaces
/// (honoring `\"` escapes), so bans only match actual code.
fn blank_string_literals(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut in_str = false;
    let mut escaped = false;
    for c in source.chars() {
        if in_str {
            if escaped {
                escaped = false;
                out.push(' ');
            } else if c == '\\' {
                escaped = true;
                out.push(' ');
            } else if c == '"' {
                in_str = false;
                out.push('"');
            } else {
                out.push(' ');
            }
        } else {
            if c == '"' {
                in_str = true;
            }
            out.push(c);
        }
    }
    out
}

/// A short display name derived from the request when the header lacks one.
fn default_name(request: &str) -> String {
    let mut words = request.split_whitespace().collect::<Vec<_>>();
    words.retain(|w| !matches!(w.to_ascii_lowercase().as_str(),
        "a" | "an" | "the" | "app" | "make" | "create" | "build" | "me" | "for" | "with"));
    let mut name = words.into_iter().take(2).collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        name = "Generated".to_string();
    }
    // Title-case each word.
    name.split_whitespace()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(18)
        .collect()
}

/// Kebab-case id from the name, made unique against the installed set.
/// Also used by the install path to re-unique against the LIVE state, since
/// the snapshot taken when the generation started can go stale (apps
/// installed while it ran, or a second generation racing this one).
pub fn unique_id(name: &str, taken: &[MiniAppId]) -> MiniAppId {
    let mut base: String = name
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if base.is_empty() {
        base = "generated".to_string();
    }
    if !taken.iter().any(|t| t == &base) {
        return base;
    }
    for n in 2.. {
        let candidate = format!("{base}-{n}");
        if !taken.iter().any(|t| t == &candidate) {
            return candidate;
        }
    }
    unreachable!()
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Evaluates `source` the exact way the Splash widget will (same prelude
/// prefix, host-only I/O) in a throwaway isolate, and returns the formatted
/// errors. Empty == the script parses and its root evaluates. Runtime errors
/// inside handlers can still happen later — this catches the compile/eval
/// class that octos-one's lint+repair loop targets, but with the real parser.
pub fn validate_splash(cx: &mut Cx, source: &str) -> Vec<String> {
    makepad_widgets::splash::validate_splash_body_with_host_io(cx, source)
}

/// A validator error as the console shows it: the app's line, the message and
/// a few suggestions, without heap ids or the validator's own source location.
/// The agent still gets the full error.
fn display_error(error: &str) -> String {
    let mut text = error.trim().to_string();
    if let Some((line, message)) = text.strip_prefix("splash:validating:")
        .and_then(|rest| rest.split_once(':'))
        .and_then(|(line, rest)| Some((line, rest.split_once(':')?.1)))
        .filter(|(line, _)| !line.is_empty() && line.bytes().all(|byte| byte.is_ascii_digit()))
    {
        text = format!("Line {line}: {}", message.trim_start());
    }
    if text.ends_with(')') && let Some(open) = text.rfind(" (") && text[open..].contains(".rs:") {
        text.truncate(open);
    }
    let mut text = without_heap_ids(&text);
    if let Some(at) = text.find(". Did you mean: ") {
        let list = &text[at + ". Did you mean: ".len()..];
        let list = list.find(" (+").map_or(list, |more| &list[..more]);
        let mut names: Vec<&str> = list.splitn(2, " or ").flat_map(|part| part.split(", ")).map(str::trim).take(3).collect();
        let last = names.pop().unwrap_or_default();
        let names = if names.is_empty() { last.to_string() } else { format!("{} or {last}", names.join(", ")) };
        text = format!("{}. Did you mean {names}?", &text[..at]);
    }
    text
}

/// Drops value previews like `(obj3848)` or `(fn325)` that only name heap slots.
fn without_heap_ids(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('(') {
        let inner = &rest[open + 1..];
        let letters = inner.bytes().take_while(u8::is_ascii_lowercase).count();
        let digits = inner[letters..].bytes().take_while(u8::is_ascii_digit).count();
        let is_heap_id = letters > 0 && digits > 0 && inner[letters + digits..].starts_with(')');
        out.push_str(&rest[..open]);
        if is_heap_id {
            rest = &inner[letters + digits + 1..];
        } else {
            out.push('(');
            rest = inner;
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// Error copy
// ---------------------------------------------------------------------------

/// First line, trimmed, capped — status-bar sized.
fn short_reason(msg: &str) -> String {
    // Scan the WHOLE message for a provider envelope, not just the first line:
    // octos reports a failure as a headline ("Internal error") with the API's
    // JSON on a later line, so a first-line-only scan threw away every word
    // that said what actually happened and showed the headline alone.
    if let Some(inner) = json_message(msg) {
        return inner;
    }
    // Nothing specific in there: hand back the WHOLE payload, not its first
    // line. It is ugly, but something ugly the user can paste into a bug
    // report beats "internal error", which is unactionable by design — and a
    // first-line-only fallback drops the JSON that agents print underneath it.
    let raw = one_line(msg);
    if raw.is_empty() { "agent error".to_string() } else { raw }
}

/// The sentence a human can act on, pulled out of a provider's error envelope.
///
/// Agents surface API failures as prose with a JSON blob stapled on:
/// `Internal error: Failed to authenticate. API Error: 403 {"error":{"type":
/// "permission_error","message":"You've reached your usage limit"}}`. The only
/// part that says what to DO is `message`, and it comes LAST — so any length
/// cap eats exactly the useful bit and leaves the punctuation. Returned in
/// full; shortening for a one-line strip is the display layer's business.
///
/// Three tiers, in order of how trustworthy the result is:
///
/// 1. **Parse it.** The live path — an ACP failure is a JSON-RPC envelope
///    built by `serde_json`, so it is always well-formed. Walking the parsed
///    tree handles nesting and escaping by construction, which hand-scanning
///    kept getting wrong.
/// 2. **Hand-scan it.** Agents hand us blobs they already truncated
///    (`…"message":"You've reached you`), which no parser will accept and
///    which are exactly the case this needs to work for.
/// 3. **Hand-scan it again, unescaped.** A payload that arrived as a *string*
///    of JSON has `\"` where a scan wants `"`. Tried last because a message
///    may legitimately contain an escaped quote, and unescaping that one cuts
///    the sentence short.
fn json_message(msg: &str) -> Option<String> {
    if let Ok(parsed) = serde_json::from_str::<Value>(msg.trim()) {
        if let Some(found) = deepest_message(&parsed) {
            return Some(found);
        }
    }
    if let Some(found) = json_message_scan(msg) {
        return Some(found);
    }
    if msg.contains("\\\"") {
        let un = msg.replace("\\\"", "\"").replace("\\n", " ");
        if let Some(found) = json_message_scan(&un) {
            return Some(found);
        }
    }
    // Nothing anywhere carried a specific sentence. Say so; `short_reason`
    // decides what to show instead — the raw payload beats a generic word.
    None
}

/// Boilerplate the transport supplies whatever went wrong. Reporting one of
/// these is reporting nothing, so they never win against a real sentence.
fn is_generic(m: &str) -> bool {
    let l = m.trim().to_ascii_lowercase();
    l == "internal error" || l == "request failed" || l == "error" || l.is_empty()
}

/// The most deeply nested human sentence in a parsed error envelope.
///
/// Depth is the ranking that matters: the transport wraps the provider, which
/// wraps the API, so the further down a string sits the more specific it is.
/// `{"code":-32603,"message":"Internal error","data":"…{\"error\":{\"message\":
/// \"quota exhausted\"}}"}` has to resolve to `quota exhausted`, three layers
/// in and behind a string that is itself a document.
fn deepest_message(v: &Value) -> Option<String> {
    let mut found: Vec<(usize, String)> = Vec::new();
    collect_messages(v, 0, &mut found);
    found
        .into_iter()
        .filter(|(_, m)| !is_generic(m))
        // Deepest first; longest breaks a tie, since siblings at one depth are
        // a terse `type` and the prose that explains it.
        .max_by_key(|(depth, m)| (*depth, m.len()))
        .map(|(_, m)| m)
}

/// Keys whose value is meant for a person to read. `type`/`code`/`param` are
/// deliberately absent: `permission_error` is not a sentence.
const MESSAGE_KEYS: [&str; 5] = ["message", "data", "details", "detail", "description"];

fn collect_messages(v: &Value, depth: usize, out: &mut Vec<(usize, String)>) {
    match v {
        Value::Object(map) => {
            for (key, val) in map {
                match val {
                    Value::String(s) => {
                        // A string that is itself an envelope — the shape octos
                        // sends. Its contents sit one level deeper than the
                        // string that carries them, so they outrank it.
                        let nested = serde_json::from_str::<Value>(s.trim())
                            .ok()
                            .and_then(|inner| deepest_message(&inner))
                            .or_else(|| json_message_scan(s));
                        if let Some(nested) = nested {
                            out.push((depth + 2, nested));
                        }
                        if MESSAGE_KEYS.contains(&key.as_str()) {
                            out.push((depth + 1, one_line(s)));
                        }
                    }
                    _ => collect_messages(val, depth + 1, out),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_messages(item, depth + 1, out);
            }
        }
        _ => {}
    }
}

fn one_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ").trim().to_string()
}

/// Every `"message"` in a blob too broken to parse, best one returned.
///
/// Scans for ALL of them, not just the first: an ACP failure is wrapped in
/// JSON-RPC, whose envelope carries a generic
/// `{"code":-32603,"message":"Internal error"}` — so taking the first match
/// reported "internal error" and threw away the provider's actual sentence,
/// which is nested deeper and therefore appears later.
fn json_message_scan(msg: &str) -> Option<String> {
    let mut found: Vec<String> = Vec::new();
    let mut rest = msg;
    while let Some(at) = rest.find(r#""message""#) {
        rest = &rest[at + r#""message""#.len() ..];
        let Some(open) = rest.find('"') else { break };
        let out = json_string_body(&rest[open + 1 ..]);
        if !out.is_empty() {
            found.push(out);
        }
        rest = &rest[open + 1 ..];
    }
    // Skip the transport's boilerplate, then take the longest — reliably the
    // provider's own explanation.
    found
        .into_iter()
        .filter(|m| !is_generic(m))
        .max_by_key(String::len)
}

/// Reads a JSON string body up to its closing quote, honouring escapes.
///
/// Escape handling is the whole point: `{\"error\":…` stuffed into a string
/// field means the first bare `"` a naive scan meets belongs to the *nested*
/// document, not to the end of this one. Stopping there cut every provider
/// error off at `HTTP 403 - {\`.
fn json_string_body(body: &str) -> String {
    let mut out = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') | Some('r') | Some('t') => out.push(' '),
                Some(esc) => out.push(esc),
                None => break,
            },
            _ => out.push(c),
        }
    }
    out.trim().to_string()
}

/// Friendlier copy for the two setup failure modes everyone will hit first:
/// the binary missing, and the provider not configured.
fn process_gone_reason(msg: &str, cmd: &str) -> String {
    if msg.contains("no LLM provider configured") {
        "No LLM provider — export an API key (e.g. ANTHROPIC_API_KEY) or run `octos init`"
            .to_string()
    } else if msg.contains("couldn't start") {
        format!("`{cmd}` isn't installed — `cargo install octos`, or add a Kimi Coding key in Providers")
    } else {
        short_reason(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2app_core::header::parse_hex_color;

    /// A generated app declares its capabilities in the header, or it can
    /// never be granted them — an undeclared capability is refused outright.
    #[test]
    fn header_carries_permissions_and_reasons() {
        let src = "// name: Sunrise\n\
                   // icon: 🌅\n\
                   // permissions: network, location, made-up-cap, network\n\
                   // why-location: Uses your city.\n\
                   // why-made-up-cap: ignored\n\
                   View{}";
        let h = parse_app_header(src);
        // Unknown ids and repeats are dropped: neither can ever be granted,
        // and both would sit in App Info promising something fake.
        assert_eq!(h.permissions, vec!["network".to_string(), "location".to_string()]);
        assert_eq!(h.permission_reasons.get("location").unwrap(), "Uses your city.");
        assert!(h.permission_reasons.get("made-up-cap").is_none());
    }

    /// No header line means no capabilities, which is the safe default for
    /// every app that never asks for anything.
    #[test]
    fn header_without_permissions_declares_nothing() {
        let h = parse_app_header("// name: Tip\nView{}");
        assert!(h.permissions.is_empty());
        assert!(h.permission_reasons.is_empty());
    }

    #[test]
    fn extracts_tagged_fence() {
        let reply = "sure!\n```splash\n// name: X\nView{}\n```\nthanks";
        assert_eq!(extract_splash_block(reply).unwrap(), "// name: X\nView{}");
    }

    #[test]
    fn extracts_longest_tagged_fence() {
        // Trailing prose that merely MENTIONS the fence must not beat the
        // real (longer) block, wherever each appears.
        let reply = "```splash\nlet a = 1\nView{}\n```\nas requested, one ```splash block";
        assert_eq!(extract_splash_block(reply).unwrap(), "let a = 1\nView{}");
        let reply2 = "a ```splash\nstub\n``` first, then\n```splash\nlet real = 1\nView{}\n```";
        assert_eq!(extract_splash_block(reply2).unwrap(), "let real = 1\nView{}");
    }

    #[test]
    fn extraction_normalizes_crlf() {
        let reply = "```splash\r\nView{}\r\n```";
        assert_eq!(extract_splash_block(reply).unwrap(), "View{}");
    }

    #[test]
    fn extracts_untagged_fallback_and_rejects_none() {
        assert_eq!(extract_splash_block("```\nBody\n```").unwrap(), "Body");
        assert!(extract_splash_block("no code here").is_none());
    }

    #[test]
    fn unterminated_fence_still_extracts() {
        let reply = "```splash\nView{}";
        assert_eq!(extract_splash_block(reply).unwrap(), "View{}");
    }

    /// Any app that still writes a fixed color is asked to convert, even a partly converted one.
    #[test]
    fn refine_converts_apps_with_fixed_colors() {
        assert!(theme_conversion("Label{draw_text +: {color: #x1C274C}}").contains("#x1C274C title"));
        assert!(!theme_conversion("Label{draw_text +: {color: theme.palette.title}} View{draw_bg +: {color: #xF0F5FF}}").is_empty());
        assert!(theme_conversion("// tint: #E84D3D\nLabel{text: \"Pomodoro\"}").is_empty());
        assert!(theme_conversion("Label{draw_text +: {color: theme.palette.title}}").is_empty());
        assert!(theme_conversion("View{draw_bg +: {color: if theme.palette.is_dark {#x2B3240} else {#xF0F5FF}}}").is_empty());
    }

    /// The guide's examples write no fixed colors, or agents would copy them.
    #[test]
    fn the_guide_teaches_no_fixed_colors() {
        let blocks: Vec<&str> = SPLASH_GUIDE.split("```splash\n").skip(1).map(|rest| rest.split("```").next().unwrap()).collect();
        assert!(blocks.len() > 10);
        for block in blocks {
            assert!(a2app_core::palette::bare_colors(block).is_empty(), "{block}");
        }
    }

    /// Only a real compile failure is called one, and fixed colors come with the shader rule.
    #[test]
    fn repair_prompts_say_what_failed() {
        let colors = bare_color_errors("// tint: #E84D3D\nView{\n    draw_bg +: {color: #x7B4FD6 border_color: #xE0E0E0}\n}");
        assert_eq!(colors, [
            "Line 3: `#x7B4FD6` won't follow the dark theme",
            "Line 3: `#xE0E0E0` won't follow the dark theme; it is the light value of `theme.palette.card_border` or `theme.palette.neutral_border`",
        ]);
        let only_colors = build_repair_prompt(&[], &colors);
        assert!(!only_colors.contains("failed to compile"));
        assert!(only_colors.contains("- Line 3: `#x7B4FD6`") && only_colors.contains("inside a shader fn"));
        let both = build_repair_prompt(&["splash:validating:2:1: unexpected token".to_string()], &colors);
        assert!(both.starts_with("Your script failed to compile. Errors:\n- splash:validating:2:1: unexpected token\n\n"));
        assert!(both.contains("- Line 3: `#xE0E0E0`"));
    }

    /// An app that compiles but writes a fixed color goes back for repair, and a run that
    /// never drops it fails saying why.
    #[test]
    fn fixed_colors_go_back_for_repair() {
        a2app_core::palette::use_light_theme();
        let mut cx = Cx::new(Box::new(|_, _| {}));
        cx.with_vm(makepad_widgets::script_mod);
        let app = |color: &str| format!("```splash\n// name: Tip\n// icon: 💰\n// tint: #4A90D9\nView{{\n    Label{{text: \"Tip\" draw_text +: {{color: {color}}}}}\n}}\n```");
        let done = |text: String| AcpEvent::TurnDone { stop_reason: "end_turn".into(), text };
        let queue = std::sync::Arc::new(std::sync::Mutex::new(vec![AcpEvent::SessionReady, done(app("#x1C274C"))]));
        let mut generation = queued(&queue);
        assert!(matches!(generation.advance(&mut cx), GenOutcome::Working));
        let console = generation.console();
        assert!(console.contains("⚠ Line 5: `#x1C274C` won't follow the dark theme; it is the light value of `theme.palette.title`"), "{console}");
        assert!(console.contains("── Repair 1: 1 error sent back ──"), "{console}");
        queue.lock().unwrap().push(done(app("theme.palette.title")));
        assert!(matches!(generation.advance(&mut cx), GenOutcome::Ready { .. }), "{}", generation.console());

        let queue = std::sync::Arc::new(std::sync::Mutex::new(vec![AcpEvent::SessionReady]));
        let mut generation = queued(&queue);
        queue.lock().unwrap().extend((0..=MAX_REPAIRS).map(|_| done(app("#x7B4FD6"))));
        let GenOutcome::Failed(reason) = generation.advance(&mut cx) else { panic!("the run gives up") };
        assert_eq!(reason, "The generated app kept using colors that don't follow the dark theme");
    }

    #[test]
    fn parses_header_comments() {
        let block = "// name: Tip Calc\n// icon: 💰\n// tint: #4A90D9\nView{}";
        let h = parse_app_header(block);
        assert_eq!(h.name.as_deref(), Some("Tip Calc"));
        assert_eq!(h.icon.as_deref(), Some("💰"));
        assert_eq!(h.tint, Some(0x4A90D9));
    }

    #[test]
    fn header_variants_parse() {
        assert_eq!(parse_hex_color("#aabbcc"), Some(0xaabbcc));
        assert_eq!(parse_hex_color("0xAABBCC"), Some(0xAABBCC));
        assert_eq!(parse_hex_color("nope"), None);
    }

    /// The real 403 that started this: the actionable sentence is at the END
    /// of a JSON blob, so the old fixed-length cut kept the HTTP status and
    /// threw away the only part that says what happened.
    #[test]
    fn an_api_error_reports_the_providers_own_message() {
        let raw = r#"Internal error: Failed to authenticate. API Error: 403 {"error":{"type":"permission_error","message":"You've reached your usage limit for this month"}}"#;
        assert_eq!(short_reason(raw), "You've reached your usage limit for this month");
    }

    /// Agents often hand us a blob that they themselves already truncated, so
    /// this can't depend on the JSON being well-formed.
    #[test]
    fn a_truncated_error_blob_still_yields_its_message() {
        let cut = r#"API Error: 403 {"error":{"type":"permission_error","message":"You've reached you"#;
        assert_eq!(short_reason(cut), "You've reached you");
        // octos reports the headline on one line and the API's JSON on the
        // next. Scanning only the first line showed "Internal error" alone.
        let octos = concat!(
            "Internal error\n",
            "API error (moonshot-coding@api/k3): provider quota exhausted - HTTP 403 - ",
            r#"{"error":{"message":"You've reached your usage limit for this billing cycle"}}"#,
        );
        assert_eq!(short_reason(octos), "You've reached your usage limit for this billing cycle");
        // The real shape: an ACP failure is wrapped in JSON-RPC, whose envelope
        // says "Internal error". Reporting THAT is reporting nothing, which is
        // exactly what the UI used to show.
        let rpc = concat!(
            r#"{"code":-32603,"message":"Internal error","data":"#,
            r#"{"error":{"type":"quota","message":"You've reached your usage limit "#,
            r#"for this billing cycle"}}}"#,
        );
        assert_eq!(
            short_reason(rpc),
            "You've reached your usage limit for this billing cycle"
        );
        // ...and when the envelope carries nothing specific, show the RAW
        // payload rather than the useless generic word.
        let bare = r#"{"code":-32603,"message":"Internal error"}"#;
        assert_eq!(short_reason(bare), bare);
        // The real shape from octos: the payload is a STRING of escaped JSON,
        // so every quote inside it arrives as \" and a naive scan misses it.
        let escaped = concat!(
            r#"prompt turn failed: API error (moonshot-coding@api/k3): "#,
            r#"provider quota exhausted - HTTP 403 - {"error":{"message":""#,
            r#"You've reached your usage limit for this billing cycle"}}"#,
        );
        assert_eq!(
            short_reason(escaped),
            "You've reached your usage limit for this billing cycle"
        );
        // A `data` string is the other place agents put the detail.
        let with_data = r#"{"code":-32603,"message":"Internal error","data":"spawn octos ENOENT"}"#;
        assert_eq!(short_reason(with_data), "spawn octos ENOENT");
        // Escapes are unwrapped rather than shown raw.
        let escaped = r#"{"message":"the \"model\" field\nis wrong"}"#;
        assert_eq!(short_reason(escaped), r#"the "model" field is wrong"#);
    }

    /// The shape that actually comes off the wire, reproduced exactly.
    ///
    /// `octos acp` reports a failed turn with `util::internal_error(…)`, which
    /// puts the whole thing in `data` AS A STRING — prose with the provider's
    /// JSON stapled on the end. Our client re-serializes that envelope, so by
    /// the time it reaches here every quote inside `data` is escaped.
    ///
    /// This is what shipped truncated at `HTTP 403 - {\`: the `"data"` scan
    /// stopped at the first bare quote it saw, which belongs to the *nested*
    /// document, and it ran before the pass that would have found the real
    /// sentence — so it won with garbage.
    #[test]
    fn the_real_octos_envelope_yields_the_providers_sentence() {
        // Built the way the client builds it, not typed by hand: an escaping
        // mistake in the fixture would make this test agree with a bug.
        let detail = concat!(
            "prompt turn failed: API error (moonshot-coding@api/k3): ",
            r#"provider quota exhausted - HTTP 403 - {"error":{"message":""#,
            r#"Your account has run out of credits; top up at kimi.com","type":"quota"}}"#,
        );
        let wire = serde_json::json!({
            "code": -32603,
            "message": "Internal error",
            "data": detail,
        })
        .to_string();
        // Precondition: the fixture really does contain the escaped nesting.
        assert!(wire.contains(r#"HTTP 403 - {\"error\""#), "fixture lost its escaping: {wire}");

        assert_eq!(
            short_reason(&wire),
            "Your account has run out of credits; top up at kimi.com"
        );
        // And nothing anywhere may end mid-escape.
        assert!(!short_reason(&wire).ends_with('\\'));
    }

    /// The literal frame `octos acp` sent, copied off the wire.
    ///
    /// Note the end: octos caps a provider's response body at 200 chars
    /// (`octos-llm/src/error.rs`), so the nested document arrives with its
    /// string UNTERMINATED — no closing quote, no closing braces. Nothing may
    /// depend on that JSON being well-formed, which is why the message scan is
    /// hand-rolled. The tail past octos's cap is gone for good; what has to
    /// survive is the sentence that says what to do about it.
    #[test]
    fn the_frame_octos_actually_sent_reads_as_a_sentence() {
        let wire = concat!(
            r#"{"code":-32603,"message":"Internal error","data":"prompt turn failed: "#,
            r#"API error (moonshot-coding@api/k3): provider quota exhausted — HTTP 403 - "#,
            r#"{\"error\":{\"message\":\"You've reached your usage limit for this billing "#,
            r#"cycle. Your quota will be refreshed in the next cycle. To continue now, "#,
            r#"purchase extra usage or upgrade your plan: https://www.kim"}"#,
        );
        assert_eq!(
            short_reason(wire),
            "You've reached your usage limit for this billing cycle. Your quota will be \
             refreshed in the next cycle. To continue now, purchase extra usage or \
             upgrade your plan: https://www.kim"
        );
    }

    /// Same envelope, but the provider gave no nested JSON — then the prose in
    /// `data` IS the answer, and it has to survive whole.
    #[test]
    fn a_data_only_envelope_keeps_the_whole_sentence() {
        let wire = serde_json::json!({
            "code": -32603,
            "message": "Internal error",
            "data": "prompt turn failed: no API key for provider `moonshot-coding` \
                     (set KIMI_CODING_API_KEY)",
        })
        .to_string();
        assert_eq!(
            short_reason(&wire),
            "prompt turn failed: no API key for provider `moonshot-coding` (set KIMI_CODING_API_KEY)"
        );
    }

    /// A plain error has no envelope and must survive untouched — and an empty
    /// one still has to say something.
    #[test]
    fn plain_errors_pass_through() {
        assert_eq!(short_reason("connection reset by peer"), "connection reset by peer");
        assert_eq!(short_reason("   "), "agent error");
    }

    #[test]
    fn bans_foreign_dialects() {
        assert_eq!(forbidden_construct("View{ sys.weather() }"), Some("sys."));
        assert_eq!(forbidden_construct("let x = 1\nView{}"), None);
    }

    #[test]
    fn ban_scan_ignores_string_literals() {
        // A label whose TEXT mentions a banned token is a legit app.
        assert_eq!(
            forbidden_construct("View{ Label{text: \"how to import  sys. stuff\"} }"),
            None
        );
        // ...but the same token as code still trips.
        assert_eq!(
            forbidden_construct("Label{text: \"hi\"}\nsys.weather()"),
            Some("sys.")
        );
        // Escaped quotes don't desync the scanner.
        assert_eq!(
            forbidden_construct("Label{text: \"a \\\" quote\"}\nimport x"),
            Some("import ")
        );
    }

    #[test]
    fn icon_header_keeps_multi_codepoint_emoji() {
        let h = parse_app_header("// icon: 👨‍👩‍👧\nView{}");
        assert_eq!(h.icon.as_deref(), Some("👨‍👩‍👧"));
    }

    #[test]
    fn ids_unique_against_taken() {
        let taken = vec!["tip-calc".to_string(), "tip-calc-2".to_string()];
        assert_eq!(unique_id("Tip Calc", &taken), "tip-calc-3");
        assert_eq!(unique_id("Fresh", &taken), "fresh");
        assert_eq!(unique_id("!!!", &[]), "generated");
    }

    #[test]
    fn default_names_from_request() {
        assert_eq!(default_name("make me a pomodoro timer app"), "Pomodoro Timer");
        assert_eq!(default_name(""), "Generated");
    }

    struct QueuedEvents(std::sync::Arc<std::sync::Mutex<Vec<AcpEvent>>>);

    impl AgentTransport for QueuedEvents {
        fn drain_events(&mut self) -> Vec<AcpEvent> { std::mem::take(&mut *self.0.lock().unwrap()) }
        fn send_prompt(&mut self, _text: &str) {}
        fn cancel(&mut self) {}
        fn desc(&self) -> &str { "queued events" }
    }

    fn queued(queue: &std::sync::Arc<std::sync::Mutex<Vec<AcpEvent>>>) -> Generation {
        Generation {
            client: Box::new(QueuedEvents(queue.clone())),
            request: "a timer".into(), mode: GenMode::Create, scope: A2AppScope::Account,
            phase: GenPhase::Connecting, repairs: 0, status: String::new(), taken_ids: Vec::new(),
            last_event: std::time::Instant::now(), slim_prompts: false,
            stream: String::new(), thought: String::new(), transcript: String::new(),
            transcript_kind: None, preview: Vec::new(), plan: Vec::new(), revision: 0, update: None,
        }
    }

    #[test]
    fn revision_changes_only_when_agent_output_arrives() {
        let queue = std::sync::Arc::new(std::sync::Mutex::new(vec![AcpEvent::SessionReady, AcpEvent::Thought("Planning".into())]));
        let mut generation = queued(&queue);
        let mut cx = Cx::new(Box::new(|_, _| {}));
        assert!(matches!(generation.advance(&mut cx), GenOutcome::Working));
        let first = generation.revision();
        assert_ne!(first, 0);
        assert!(generation.console().contains("Planning"));
        // An idle pass leaves the console nothing new to draw.
        generation.advance(&mut cx);
        assert_eq!(generation.revision(), first);
        queue.lock().unwrap().push(AcpEvent::Chunk("```splash".into()));
        generation.advance(&mut cx);
        assert!(generation.revision() > first);
        assert_eq!(generation.status_line(), "Writing the app…");
    }

    #[test]
    fn console_reads_in_time_order_and_previews_give_way_to_the_reply() {
        let queue = std::sync::Arc::new(std::sync::Mutex::new(vec![
            AcpEvent::SessionReady,
            AcpEvent::Preview { is_thought: true, text: "Plan the ".into() },
            AcpEvent::Preview { is_thought: true, text: "layout".into() },
        ]));
        let mut generation = queued(&queue);
        let mut cx = Cx::new(Box::new(|_, _| {}));
        generation.advance(&mut cx);
        let thinking = "Agent connected\n\n💭 Thinking\nPlan the layout";
        assert_eq!(generation.console(), thinking);
        assert_eq!(generation.status_line(), "Thinking…");

        // The agent's own copy of the streamed reply replaces its preview.
        queue.lock().unwrap().push(AcpEvent::Thought("Plan the layout".into()));
        generation.advance(&mut cx);
        assert_eq!(generation.console(), thinking);

        // A later log line lands below the output that came before it.
        queue.lock().unwrap().extend([
            AcpEvent::ToolCall { id: "call-1".into(), title: "Read the guide".into() },
            AcpEvent::Preview { is_thought: false, text: "```splash\n".into() },
        ]);
        generation.advance(&mut cx);
        assert_eq!(generation.status_line(), "Writing the app…");
        assert_eq!(generation.console(), format!("{thinking}\n\n🔧 Read the guide\n\n✍️ Writing\n```splash\n"));
        queue.lock().unwrap().push(AcpEvent::Chunk("```splash\nView{}\n```".into()));
        generation.advance(&mut cx);
        assert_eq!(generation.console(), format!("{thinking}\n\n🔧 Read the guide\n\n✍️ Writing\n```splash\nView{{}}\n```"));
    }

    #[test]
    fn the_agents_copy_replaces_each_previewed_kind_and_a_failed_run_drops_the_rest() {
        let queue = std::sync::Arc::new(std::sync::Mutex::new(vec![
            AcpEvent::SessionReady,
            AcpEvent::Preview { is_thought: true, text: "Plan".into() },
            AcpEvent::Preview { is_thought: false, text: "```splash\nView{}\n```".into() },
        ]));
        let mut generation = queued(&queue);
        let mut cx = Cx::new(Box::new(|_, _| {}));
        generation.advance(&mut cx);
        let streamed = "Agent connected\n\n💭 Thinking\nPlan\n\n✍️ Writing\n```splash\nView{}\n```";
        assert_eq!(generation.console(), streamed);
        // The copied thinking alone leaves the streamed code in place, and the status on it.
        queue.lock().unwrap().push(AcpEvent::Thought("Plan".into()));
        generation.advance(&mut cx);
        assert_eq!(generation.console(), streamed);
        assert_eq!(generation.status_line(), "Writing the app…");
        queue.lock().unwrap().push(AcpEvent::Chunk("```splash\nView{}\n```".into()));
        generation.advance(&mut cx);
        assert_eq!(generation.console(), streamed);

        // Output the agent never confirmed leaves with a failed run.
        queue.lock().unwrap().extend([
            AcpEvent::Preview { is_thought: false, text: "\n// name: Half".into() },
            AcpEvent::ProcessGone("agent process exited".into()),
        ]);
        assert!(matches!(generation.advance(&mut cx), GenOutcome::Failed(_)));
        assert_eq!(generation.console(), streamed);
    }

    #[cfg(unix)]
    #[test]
    fn an_older_claude_code_gets_one_update_notice_and_a_current_one_none() {
        let _lock = crate::CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        struct Restore(Vec<(&'static str, Option<std::ffi::OsString>)>);
        impl Drop for Restore { fn drop(&mut self) { for (name, value) in &self.0 { unsafe { match value { Some(value) => std::env::set_var(name, value), None => std::env::remove_var(name) } } } } }
        let _restore = Restore(["CLAUDE_CODE_EXECUTABLE", "CLAUDE_CONFIG_DIR"].iter().map(|name| (*name, std::env::var_os(name))).collect());
        // The managed policy check reads no real Claude configuration.
        let config = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", config.path()); }
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let partial = "  --include-partial-messages  Include partial message chunks as they arrive";
        let with_thinking = |thinking: Option<&str>| AgentPrefs { thinking: thinking.map(str::to_string), ..Default::default() };
        let claude_code = Some(crate::claude_code::ID);
        let no_thinking = "Claude Code 2.1.63 shows the app as it's written, but not newer models' thinking. Run `claude update` in a terminal to see that too.";
        for (help, version, provider, prefs, notice) in [
            // Even with thinking off, this build can't show the reply live.
            ("  --output-format <format>  Output format", "1.0.30 (Claude Code)", claude_code, with_thinking(Some("off")),
                Some("Claude Code 1.0.30 can't show the model's output live. Run `claude update` in a terminal to see it as it's written.")),
            // Unset, the CLI's own default may think.
            (partial, "2.1.63 (Claude Code)", claude_code, with_thinking(None), Some(no_thinking)),
            (partial, "2.1.63 (Claude Code)", claude_code, with_thinking(Some("on")), Some(no_thinking)),
            // With thinking switched off there is no thinking to miss.
            (partial, "2.1.63 (Claude Code)", claude_code, with_thinking(Some("off")), None),
            // A run on another provider never asks the CLI.
            (partial, "2.1.63 (Claude Code)", Some("anthropic"), with_thinking(Some("on")), None),
            (partial, "2.1.301 (Claude Code)", claude_code, with_thinking(Some("on")), None),
        ] {
            let case = format!("{version} on {provider:?}, thinking {:?}", prefs.thinking);
            let directory = tempfile::tempdir().unwrap();
            // Its help waits to be released, and it notes what it is asked.
            let cli = crate::claude_code::fixture_cli(directory.path(), &format!("here=\"${{0%/*}}\"\ncase \"$1\" in\n\
                --version) printf '%s ' \"$1\" >> \"$here/asked\"; printf '%s\\n' '{version}' ;;\n\
                --help) printf '%s ' \"$1\" >> \"$here/asked\"; while [ ! -e \"$here/release\" ]; do /bin/sleep 0.02; done; printf '%s\\n' '{help}' ;;\n\
                esac\n"));
            unsafe { std::env::set_var("CLAUDE_CODE_EXECUTABLE", &cli); }
            let mut generation = queued(&std::sync::Arc::new(std::sync::Mutex::new(vec![AcpEvent::SessionReady])));
            let started = std::time::Instant::now();
            generation.update = check_update(provider, &prefs);
            for _ in 0..3 { generation.advance(&mut cx); }
            // The UI never waits for the check, which can't finish yet.
            assert!(started.elapsed() < std::time::Duration::from_secs(1), "{case}: the UI waited for the CLI");
            assert_eq!(generation.console(), "Agent connected", "{case}");
            std::fs::write(directory.path().join("release"), "").unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while generation.update.is_some() {
                generation.advance(&mut cx);
                assert!(std::time::Instant::now() < deadline, "{case}: the update check never finished");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            for _ in 0..3 { generation.advance(&mut cx); }
            let console = generation.console();
            match notice {
                Some(notice) => assert_eq!(console.lines().filter(|line| *line == notice).count(), 1, "{case}: {console}"),
                None => assert_eq!(console, "Agent connected", "{case}"),
            }
            let asked = std::fs::read_to_string(directory.path().join("asked")).unwrap_or_default();
            assert_eq!(asked, if provider == claude_code { "--help --version " } else { "" }, "{case}");
        }
    }

    #[test]
    fn validator_errors_read_as_one_plain_line() {
        let raw = "splash:validating:6:21: variable DefinitelyNotAWidget not found in scope. Did you mean: SplitterAlign(obj3848) or Sentences(obj1134), texture_video(fn325), Slider(obj12), Splash(obj9) (+626 more) (/Users/someone/makepad/platform/script/src/object_heap.rs:726)";
        assert_eq!(display_error(raw), "Line 6: variable DefinitelyNotAWidget not found in scope. Did you mean SplitterAlign, Sentences or texture_video?");
        assert_eq!(display_error("splash:validating:2:1: unexpected token"), "Line 2: unexpected token");
        assert_eq!(display_error("script has no root widget (e.g. View{...}) as its final expression"),
            "script has no root widget (e.g. View{...}) as its final expression");
        assert_eq!(display_error("unknown property foo. Did you mean: font(obj3)"), "unknown property foo. Did you mean font?");
    }
}
