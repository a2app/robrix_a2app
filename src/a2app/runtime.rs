//! The a2app runtime glue: owns all mini-app state (registry, grants,
//! prompts, the in-flight generation) and drives the host-service broker
//! once per event pass, mirroring how host_launcher's `App` did it.
//!
//! Widgets read this state through [`with_a2app`] and mutate it by emitting
//! [`A2AppOp`] actions, which [`process`] applies centrally.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use makepad_widgets::*;
use makepad_widgets::splash_host::SplashHostRequest;
use matrix_sdk::RoomState;
use matrix_sdk::ruma::{matrix_uri::MatrixId, MatrixToUri, MatrixUri, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId};

use a2app_core::builtin;
use a2app_core::bundle;
use a2app_core::manifest::{A2AppScope, AppRegistry, MiniAppId, MiniAppManifest, RunsIn};
use a2app_core::permissions::{
    Effective, GrantState, Permission, PermissionStore, agent_subject, is_agent_subject,
    GrantDuration, NetworkScope, PermissionContext, PolicyDecision, RoomAccess, RoomPolicyMode, RoomScope,
};
#[cfg(unix)]
use a2app_core::permissions::agent_room_of;
use a2app_core::persistence::{self, A2AppPersistedState};
use a2app_core::services::{
    self, Broker, BrokerAsk, BrokerCtx, HostAction, HostQuery, Reply, MATRIX_WRITE_OFF_MSG,
};
#[cfg(unix)]
use a2app_core::services::AppToolRequest;
use a2app_core::versions::{self, AcquisitionSource, VersionActor, VersionOrigin};
use a2app_agent::intent::Intent;
use a2app_agent::pipeline::{GenOutcome, Generation};
use a2app_agent::prefs::AgentPrefs;

use crate::a2app::host_pane::{MiniAppHostPaneAction, MiniAppHostPaneWidgetRefExt};
use crate::a2app::permission_prompt::{
    MiniAppPermissionPromptWidgetRefExt, PermissionPromptAction, PermissionPromptResponse, PromptInfo, FlowPromptInfo, ToolPreview,
    PermissionPromptGroupInfo, PermissionPromptGroupResponse, PermissionPromptInfo,
};
mod permission_batch;
pub(super) mod permission_lifecycle;
use crate::room::room_pane::{self, RoomPaneKind, RoomPaneOp};
use crate::a2app::instances::{self, MiniAppInstanceAction, Surface};
use crate::a2app::matrix::{self, A2AppMatrixRequest, A2AppMatrixResult};
use crate::a2app::room_watch::{A2AppRoomWatchEvent, RoomWatchKind};
use crate::a2app::account_watch::{A2AppAccountWatchEvent, AccountWatchKind, ACCOUNT_HOOKS};
use crate::app::{AppStateAction, SelectedRoom};
use crate::home::navigation_tab_bar::NavigationBarAction;
use crate::home::rooms_list::{RoomsListAction, RoomsListRef};
use crate::room::BasicRoomDetails;
use crate::home::home_screen::effective_is_desktop;
use crate::settings::app_preferences::{AppPreferencesAction, AppPreferencesGlobal, ThumbnailMaxHeight, ViewModeOverride};
use crate::shared::popup_list::{enqueue_popup_notification, PopupKind};
use crate::sliding_sync::{submit_async_request, MatrixRequest};
use crate::utils::RoomNameId;

#[cfg(unix)]
use a2app_core::information_flow::{Recipient, Source};
#[cfg(unix)]
use a2app_core::task_grants::{self, ItemState, ResolveInputs, TaskLedger, TaskPlan, TaskReason};
#[cfg(unix)]
use crate::a2app::ai::session::{AiSession, PromptOutcome, SessionJob, SessionUpdate};
#[cfg(unix)]
use crate::a2app::task_permission_prompt::{
    TaskItemView, TaskPermissionAction, TaskPermissionPromptWidgetRefExt, TaskPromptInfo,
};
#[cfg(unix)]
use crate::a2app::ai::rooms::{next_ai_state_key, AiRoomAction, AiRoomRequest};
#[cfg(unix)]
use crate::a2app::ai::tools::{AI_ROOM_SESSION_CAP_IDS, LAUNCH_APP_CAP_ID, LIST_APPS_CAP_ID, ReadToolKind, read_tool_name};
#[cfg(unix)]
use crate::a2app::ai_room_panel::{
    AiRoomPanelAction, AiRoomPanelCommand, AiRoomPanelInfo, AiRoomPanelWidgetRefExt,
};
#[cfg(unix)]
use crate::a2app::ai_room_events::{
    AI_ACTIVITY_EVENT_TYPE, AI_TURN_EVENT_TYPE, AiActivityContent, AiActivityKind, AiReplyContent,
    AiReplyToolCall, AiTurnContent, AiTurnStatus, AiTurnToolCall, AiTurnToolStatus,
};
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(unix)]
use std::sync::mpsc::Sender;

/// How long between saves of dirty permission/registry state.
const PERSIST_THROTTLE: Duration = Duration::from_secs(2);
/// How often expired timed grants are checked for.
const TIMED_GRANT_CHECK: Duration = Duration::from_secs(5);
/// How often the generation console re-renders while output streams in.
const CONSOLE_REPAINT: Duration = Duration::from_millis(120);
/// The heartbeat interval kept alive while a generation runs, so its stall
/// watchdog can fire without the agent's own (now-silent) events. Cheap: a
/// timer tick just drains an empty event queue when all is well.
const STALL_HEARTBEAT_SECS: f64 = 5.0;
/// How long a room-scoped host action waits for its RoomScreen to appear.
const ROOM_ACTION_TTL: Duration = Duration::from_secs(10);
/// Read tool calls a session may make per rolling window before it is refused
/// until the window rolls: an agent legitimately batches a few reads a turn;
/// far more than that is a loop, and each refused call also tells the model to
/// stop.
#[cfg(unix)]
const AI_READ_BUDGET: u32 = 24;
#[cfg(unix)]
const AI_READ_WINDOW: Duration = Duration::from_secs(10);
/// How long a failed agent start is left alone before a new member message
/// tries again.
#[cfg(unix)]
const AI_START_RETRY_COOLDOWN: Duration = Duration::from_secs(60);
/// The next id for a granted AI-room read request, so the async worker's
/// result can find the exact tool call it answers.
#[cfg(unix)]
static NEXT_AI_TOOL_ID: AtomicU64 = AtomicU64::new(1);
/// The next id for an `ai_turn`/activity state-event write. The id is echoed
/// back on its result so a closed turn's final write can be matched to the
/// token it registered.
#[cfg(unix)]
static NEXT_AI_WRITE_ID: AtomicU64 = AtomicU64::new(1);
/// How long a closed turn's grants may outlive a final write that never
/// reports back before the runtime gives up and revokes them anyway.
#[cfg(unix)]
const FINAL_WRITE_FAILSAFE: Duration = Duration::from_secs(30);

/// One granted read in flight: its room, the tool kind (so the receipt chip
/// can name it when the result lands) and the channel answering the tool call.
#[cfg(unix)]
type AiReadPending = (OwnedRoomId, ReadToolKind, Sender<Result<String, String>>);
/// What one in-flight cross-room post is waiting on (see `AiRoomInfo`'s
/// sibling `ai_posts` map): the session room (for the receipt) and the
/// channel that must answer the parked tool call when the async worker's
/// [`AiRoomAction::PostToRoomResult`] lands.
#[cfg(unix)]
type AiPostPending = (OwnedRoomId, Sender<Result<String, String>>);

/// One `send_message` tool call waiting on its `ai_reply` write: the room and
/// the turn that made the call, the channel that answers it, and the turn's
/// receipts the write consumed — handed back if it fails, so they still ride
/// that turn's card. The turn is recorded because the write outlives a turn
/// the user cancels, and its result must not touch the next one.
#[cfg(unix)]
type AiReplyPending = (
    OwnedRoomId,
    Option<String>,
    Sender<Result<String, String>>,
    Vec<AiReplyToolCall>,
);

/// Next id for an app-tool invocation: the runtime-generated `call_id` handed
/// to the isolate in `on_tool_call`, which the app echoes back with its
/// `mcp.tools.result` so the exact waiting serve thread can be answered.
#[cfg(unix)]
static NEXT_APP_TOOL_CALL_ID: AtomicU64 = AtomicU64::new(1);

/// How long an app-tool invocation waits for the app's `mcp.tools.result`
/// before the model is told it timed out. Comfortably under octos's 60s
/// `tools/call` cancel, and long enough for a user to click something.
#[cfg(unix)]
const APP_TOOL_TIMEOUT: Duration = Duration::from_secs(20);

/// A mini-app tool installed on one room's agent session: where to route an
/// invocation and what to withdraw if the owning instance goes away.
#[cfg(unix)]
pub struct AppToolRegistration {
    /// The namespaced name the model sees (also the map key).
    pub full_name: String,
    /// The app's own chosen name, so `on_tool_call` can route without knowing
    /// the runtime-assigned namespace prefix.
    pub raw_name: String,
    /// The room whose session serves the tool.
    pub room_id: OwnedRoomId,
    pub app_id: MiniAppId,
    /// The isolate that registered it (an app may have several instances).
    pub heap_key: usize,
    /// The app-authored description and args, kept for the invocation prompt.
    pub description: String,
    /// The validated JSON Schema, so a restarted session can re-install the
    /// tool without re-review.
    pub schema: serde_json::Value,
    pub args: Vec<(String, String, String)>,
}

/// One app-tool invocation waiting on the app's `mcp.tools.result`, keyed by
/// the `call_id` the runtime gave the isolate.
#[cfg(unix)]
pub struct AppToolPending {
    pub room_id: OwnedRoomId,
    pub app_id: MiniAppId,
    pub heap_key: usize,
    /// The namespaced name the runtime routes by (also the app-tool key).
    pub full_name: String,
    /// The name the agent's turn card used for this call: the namespaced name
    /// when the model called the tool directly, or `call_mini_app_tool` when
    /// it came through the stable bridge. The finishing update must close the
    /// row the agent actually opened.
    pub display_name: String,
    pub answer: Sender<Result<String, String>>,
    pub since: Instant,
    pub audit: a2app_core::protection_audit::Attempt,
}

thread_local! {
    static A2APP: RefCell<Option<A2AppState>> = const { RefCell::new(None) };
}

/// Runs `f` against the global a2app state, if it has been initialized.
///
/// The state is a `RefCell`, so calling this while an outer [`with_a2app`]
/// borrow is still live panics. That nesting is a bug no matter where it
/// happens, but the bare "RefCell already borrowed" message names neither
/// side of it — so on re-entry the panic carries a backtrace of the SECOND
/// borrow, whose stack still contains the first (still-unwound) [`with_a2app`]
/// frame. Works without `RUST_BACKTRACE=1`.
pub fn with_a2app<R>(f: impl FnOnce(&mut A2AppState) -> R) -> Option<R> {
    A2APP.with(|state| {
        let mut guard = state.try_borrow_mut().unwrap_or_else(|_| {
            panic!(
                "a2app state re-entered while already borrowed (the outer \
                 with_a2app frame is still on this stack):\n{}",
                std::backtrace::Backtrace::force_capture()
            )
        });
        guard.as_mut().map(f)
    })
}

/// What can be parked behind one permission prompt: a mini-app bridge request
/// (replayed through the broker once the user answers) or an AI-room agent
/// tool call (re-decided and executed / refused once the user answers).
pub enum ParkedRequest {
    /// A mini-app request waiting on its group's answer (`None` when the
    /// prompt was raised for a group without a specific request behind it).
    Bridge(Option<SplashHostRequest>),
    /// An AI session tool call waiting on its room subject's answer.
    #[cfg(unix)]
    AiTool { room_id: OwnedRoomId, job: SessionJob },
    /// One of the agent's own internet tools wants to reach `host`/`url`:
    /// parked until the user allows (or refuses) that host for this room's
    /// AI. Unlike a tool call, the allowance is per host, so an answer here
    /// records a host, not a group.
    #[cfg(unix)]
    NetworkAccess {
        room_id: OwnedRoomId,
        host: String,
        url: String,
        answer: Sender<Result<String, String>>,
    },
}

/// The exact tool an `mcp-tools` prompt is about: the app-authored text the
/// modal shows the user, and what the answer records as a per-tool grant.
#[derive(Clone, Debug)]
pub struct PromptToolGrant {
    pub full_name: String,
    pub name: String,
    /// The app-authored description, verbatim — exactly what the model sees.
    pub description: String,
    pub args: Vec<(String, String, String)>,
    /// Registration content hash (empty for an invocation grant).
    pub content_hash: String,
}

/// A runtime permission prompt waiting for (or showing to) the user.
pub struct PermissionPrompt {
    id: u64,
    setup: Option<(usize, u64)>,
    /// The permission-store subject the answer is recorded for: an installed
    /// app's id, or an AI room's agent key (see [`agent_subject`]). One
    /// decision, one subject — a mini-app and a room's AI can never answer
    /// for each other.
    pub subject: String,
    pub perm: Permission,
    /// Requests parked behind this prompt; replayed or refused once the user
    /// answers.
    pub parked: Vec<ParkedRequest>,
    /// For an `mcp-tools` prompt: the tool under review.
    pub tool: Option<PromptToolGrant>,
    flow: Option<FlowContinuation>,
    enable_writes: bool,
    activations: Vec<(usize, u64, a2app_core::information_flow::ContextId, u64)>,
}

static NEXT_PERMISSION_PROMPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_permission_prompt_id() -> u64 {
    NEXT_PERMISSION_PROMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

enum FlowContinuation {
    Bridge { request: SplashHostRequest, review: a2app_core::information_flow::EffectReview, allow_once: bool, permission: Option<Permission> },
    Worker(super::effect_review::WorkerReview),
    Generated { review: a2app_core::information_flow::EffectReview },
}

impl FlowContinuation {
    fn review(&self) -> &a2app_core::information_flow::EffectReview {
        match self { Self::Bridge { review, .. } | Self::Generated { review } => review, Self::Worker(worker) => &worker.review }
    }

    fn allow_once(&self) -> bool {
        match self { Self::Bridge { allow_once, .. } => *allow_once, Self::Worker(worker) => worker.allow_once, Self::Generated { .. } => true }
    }

    fn is_cancelled(&self) -> bool {
        match self {
            Self::Worker(worker) => worker.is_cancelled(),
            Self::Generated { .. } => !self.can_prompt(),
            Self::Bridge { .. } => false,
        }
    }

    fn can_prompt(&self) -> bool {
        with_a2app(|state| self.can_prompt_with_state(state)).unwrap_or(false)
    }

    fn can_prompt_with_state(&self, state: &A2AppState) -> bool {
        let review = self.review();
        if super::information_flow::current_context(&review.context).is_err()
            || a2app_core::information_flow::ensure_context_epoch(&review.context, review.epoch).is_err()
        { return false; }
        match self {
            Self::Generated { .. } => state.pending_generated.as_ref().is_some_and(|pending|
                pending.matches_review(review)
                    && state.generation_context.as_ref() == Some(&review.context) && state.generation_epoch == Some(review.epoch)
                    && pending.can_retain()),
            _ => instances::context_can_prompt(&review.context, review.epoch),
        }
    }

    fn replace_review(&mut self, mut updated: a2app_core::information_flow::EffectReview) {
        // An allowed capture has no ticket payload. Keep the complete request
        // available when capability consent is still waiting in this modal.
        if updated.payload.is_empty() { updated.payload = self.review().payload.clone(); }
        match self {
            Self::Generated { review } => {
                with_a2app(|state| {
                    if let Some(pending) = state.pending_generated.as_mut().filter(|pending|
                        pending.matches_review(review))
                    { pending.effect_review_id = Some(updated.id); }
                });
                *review = updated;
            }
            Self::Bridge { review, .. } => *review = updated,
            Self::Worker(worker) => worker.review = updated,
        }
    }

    fn approve(&self, approve: impl FnOnce(&a2app_core::information_flow::EffectReview) -> Result<(), String>) -> Result<(), String> {
        match self { Self::Bridge { review, .. } | Self::Generated { review } => approve(review), Self::Worker(worker) => worker.approve(approve) }
    }
}

/// One resolved task plan waiting to be shown, or showing, as its own modal:
/// the agent's whole request for a task, applied atomically once answered.
#[cfg(unix)]
pub struct TaskPrompt {
    pub room_id: OwnedRoomId,
    pub plan: TaskPlan,
    /// What to do once the user answers. An agent's own request releases the
    /// parked `request_task_permissions` tool call; a carried-over prompt the
    /// host raised on the agent's behalf releases the user messages it held
    /// back so the turn can start.
    pub resume: TaskResume,
}

/// What a task prompt does once the user answers it.
#[cfg(unix)]
pub enum TaskResume {
    /// Release the `request_task_permissions` tool call that raised it.
    AgentTool(Sender<Result<String, String>>),
    /// Deliver the held user messages; the turn then runs with whatever the
    /// user approved (on a decline, the model call is refused as usual).
    UserPrompt { texts: Vec<(OwnedEventId, String)> },
}

/// A parked agent effect waiting on the exact-action review. The same modal
/// that shows task plans shows the exact target and host-captured payload;
/// Allow grants the captured once-authority and resumes the effect, Not now
/// refuses it.
#[cfg(unix)]
pub struct ExactReviewPrompt {
    pub room_id: OwnedRoomId,
    pub context: a2app_core::information_flow::ContextId,
    pub epoch: u64,
    pub action: a2app_core::information_flow::SensitiveAction,
    pub payload: serde_json::Value,
    pub expected: a2app_core::information_flow::Influences,
    pub request_id: u64,
    pub info: TaskPromptInfo,
    pub resume: ExactResume,
}

/// What to run once an exact-action review is approved.
#[cfg(unix)]
pub enum ExactResume {
    /// Re-run the parked tool job (a cross-room post or an app generation).
    Job(SessionJob),
    /// Re-run an app-tool invocation after its provenance transfers; the
    /// transfers are idempotent, so the commit matches the captured set.
    AppTool {
        tool: String,
        arguments: serde_json::Map<String, serde_json::Value>,
        display_name: String,
        answer: Sender<Result<String, String>>,
    },
}

/// The state of the AI generation console shown in the Mini Apps screen.
#[derive(Default)]
pub struct GenConsole {
    pub status: String,
    pub lines: Vec<String>,
    /// The blocked generation's compartment, opened by Review permissions.
    pub review_context: Option<a2app_core::information_flow::ContextId>,
    /// True from submit until the user starts a new prompt.
    pub active: bool,
    pub last_render: Option<Instant>,
}

impl GenConsole {
    fn failed(&mut self, reason: &str, context: Option<a2app_core::information_flow::ContextId>) {
        use a2app_core::information_flow as flow;
        let needs_review = [
            "Your permission is needed before this data can be sent.",
            "Choose where to send this data before allowing this request.",
            flow::ACTION_REVIEW_REQUIRED,
            flow::EFFECT_REVIEW_REQUIRED,
        ].iter().any(|message| reason.contains(message));
        self.review_context = context.filter(|_| needs_review);
        self.status = format!("Failed: {reason}");
        if needs_review {
            self.status.push_str(if self.review_context.is_some() {
                " Select Review permissions to open Agent permissions → Needs attention. Review and approve the blocked request, then return here and select Retry."
            } else {
                " Open Agent permissions → Needs attention to review the blocked request, then return here and select Retry."
            });
        }
    }
}

#[cfg(test)]
mod generation_recovery_tests {
    use super::*;
    use a2app_core::information_flow::{self as flow, ContextId};

    #[test]
    fn blocked_generation_explains_the_review_route_and_keeps_its_room() {
        let context = ContextId::Agent { account: "@tester:example.org".into(), room: "!testing:example.org".into() };
        for reason in [
            "agent turn failed: Your permission is needed before this data can be sent.",
            "Choose where to send this data before allowing this request.",
            flow::ACTION_REVIEW_REQUIRED,
            flow::EFFECT_REVIEW_REQUIRED,
        ] {
            let mut console = GenConsole::default();
            console.failed(reason, Some(context.clone()));
            assert_eq!(console.review_context.as_ref(), Some(&context));
            assert!(console.status.contains("Select Review permissions"));
            assert!(console.status.contains("Agent permissions → Needs attention"));
            assert!(console.status.contains("select Retry"));
        }
    }

    #[test]
    fn non_permission_failure_clears_the_old_review_route() {
        let mut console = GenConsole {
            review_context: Some(ContextId::App { account: "tester".into(), app: "__generation".into(), room: None }),
            ..Default::default()
        };
        console.failed("The agent connection closed.", console.review_context.clone());
        assert!(console.review_context.is_none());
        assert_eq!(console.status, "Failed: The agent connection closed.");
    }

    #[test]
    fn stopped_generation_explains_the_management_route_without_a_direct_review_button() {
        let mut console = GenConsole::default();
        console.failed(flow::EFFECT_REVIEW_REQUIRED, None);
        assert!(console.review_context.is_none());
        assert!(console.status.contains("Open Agent permissions → Needs attention"));
        assert!(!console.status.contains("Select Review permissions"));
    }
}

/// The turn currently open for one AI room: the tool calls it has made so
/// far and the state key of the single `ai_turn` row they are aggregated
/// into. Created on the turn's first tool call and closed when the turn's
/// reply (or error) lands.
#[cfg(unix)]
struct ActiveTurn {
    /// The state key of the posted `ai_turn` row, reused for every rewrite so
    /// the timeline shows one in-place card per turn.
    key: String,
    /// The turn's tool calls, in the order they started.
    tool_calls: Vec<AiTurnToolCall>,
    /// Whether the model is reasoning on this turn right now (a `Thought`
    /// stream is live), rendered as the card's leading body line.
    thinking: bool,
    /// Monotonic snapshot counter, bumped on every rewrite. The writes race
    /// over the network, so this is what lets the renderer pick the newest
    /// snapshot regardless of the order the server applied them in.
    seq: u64,
    /// Milliseconds since the epoch, fixed for the turn and reused on every
    /// rewrite so the row's timestamp is stable.
    created_at: u64,
}

/// An AI room's marker/forwarding state, tracked once the room is opened.
#[cfg(unix)]
pub struct AiRoomInfo {
    /// The last member message successfully forwarded to this room's
    /// session, also persisted as its `ai_session_data` account-data
    /// cursor. Only events after this one are forwarded.
    pub cursor: Option<OwnedEventId>,
    /// Whether a `send_message` tool call posted to the room during the
    /// session's current turn. When that turn's final text arrives it is
    /// redundant (the tool already said it), so the runtime drops it — a
    /// tool turn must leave exactly one `ai_reply`. Cleared when the turn
    /// ends (its reply is consumed, or the turn errors out).
    posted_by_tool_this_turn: bool,
    /// Read-tool flood guard: calls made within the current rolling window
    /// (see `AI_READ_BUDGET` / `AI_READ_WINDOW`). A session that reads far
    /// faster than any conversation could is a loop; it is refused until the
    /// window rolls over.
    read_burst: u32,
    read_window_started: Option<Instant>,
    /// Whether this room's AI is turned on at all (the panel's power switch).
    /// Off stops the session and forwarding; the permission grants are kept.
    session_on: bool,
    /// Tool calls this turn actually executed (or were refused), waiting to
    /// ride on the turn's one `ai_reply` card as a receipt. Cleared when the
    /// card is posted or the turn errors out.
    pending_tool_calls: Vec<AiReplyToolCall>,
    /// The turn's single aggregated tool-call card, open while the agent is
    /// working and closed when its reply (or error) lands. `None` before the
    /// turn's first tool call and between turns.
    active_turn: Option<ActiveTurn>,
    /// `ai_turn` snapshots waiting to be written, coalesced per turn: a burst
    /// of turn activity (thinking, each tool start/detail/finish) keeps only
    /// the turn's latest snapshot here until the previous write completes and
    /// the rate-limit cooldown passes, while a previous turn's final snapshot
    /// stays queued ahead of it. One state-event write per snapshot is then
    /// sent, instead of one per update — matrix.org rate-limits state events
    /// hard (429 M_LIMIT_EXCEEDED), and a turn can otherwise produce dozens.
    pending_ai_turns: VecDeque<(String, AiTurnContent)>,
    /// Whether an `ai_turn` write for this room is in flight. Cleared by
    /// [`AiRoomAction::StateEventPosted`]; no further `ai_turn` write is sent
    /// while it is set, so the SDK's own 429 retries are not compounded.
    ai_turn_in_flight: bool,
    /// The submitted snapshot whose result may release the current gate.
    /// Results from a stopped activation must not release its replacement.
    ai_turn_write_id: Option<u64>,
    /// When this room's last `ai_turn` write was submitted, so writes are
    /// spaced under the server's state-event rate limit.
    last_ai_turn_post: Option<Instant>,
    /// The current minimum spacing between this room's `ai_turn` writes. It
    /// starts at [`AI_TURN_POST_MIN_INTERVAL`], doubles (up to
    /// [`AI_TURN_POST_MAX_INTERVAL`]) whenever the homeserver rejects a write
    /// as rate-limited, and halves back down on success — so it converges on
    /// whatever state-event budget the server actually enforces instead of
    /// relying on a hard-coded guess.
    ai_turn_backoff: Duration,
    /// What the room's busy/queued status row last showed. Compared against
    /// the live session each event pass, so the row redraws only when its
    /// state actually changes (and hides once the agent goes idle).
    status_busy: bool,
    status_queued: usize,
    /// Member messages wait in the host until the preceding turn's final
    /// writes settle and its grants are revoked. Held events keep their IDs
    /// and do not advance the cursor, so a restart may refetch them.
    pending_member_prompts: VecDeque<(OwnedEventId, String)>,
    /// The turn whose first `ai_turn` snapshot has been written: the card's
    /// anchor row, decided at flush time (see `flush_pending_ai_turns`).
    first_posted_turn: Option<String>,
    /// The turn whose anchor write is in flight, so a failed write can give
    /// the anchor back to the turn's next snapshot.
    anchor_in_flight: Option<String>,
    /// When starting this room's agent last failed, so a failure is retried
    /// after a cooldown instead of on every timeline update.
    start_failed_at: Option<Instant>,
    /// A closed turn's final writes (the `Done` `ai_turn` snapshot, an
    /// error/stopped activity row, the natural reply) still queued or in
    /// flight, each by its host-assigned write token. The turn's task-scoped
    /// grants stay live until every token is released, so each of those writes
    /// still passes the whole-label sharing check. See [`close_active_turn`].
    pending_final_writes: BTreeSet<u64>,
    /// Whether the turn is closed and only its final writes keep its grants
    /// alive. Set by [`close_active_turn`], cleared when the revoke runs.
    final_writes_pending_revoke: bool,
    /// Final `ai_turn` (Done) snapshots still queued for closed turns. Each
    /// must be submitted before [`settle_closed_turn`] may revoke, including
    /// when several short turns close before the next flush.
    pending_final_turns: BTreeSet<String>,
    /// When the closed turn's grants were parked. If a final write never
    /// reports back, [`flush_pending_ai_turns`] revokes at this point so a
    /// stuck write cannot keep grants alive forever.
    final_writes_deadline: Option<Instant>,
    /// Whether a dead session's teardown is waiting for its final writes to
    /// land before it removes the flow context (`Gone`).
    retire_after_final_writes: bool,
}

#[cfg(unix)]
impl AiRoomInfo {
    fn new(cursor: Option<OwnedEventId>) -> Self {
        Self {
            cursor,
            posted_by_tool_this_turn: false,
            read_burst: 0,
            read_window_started: None,
            session_on: true,
            pending_tool_calls: Vec::new(),
            active_turn: None,
            pending_ai_turns: VecDeque::new(),
            ai_turn_in_flight: false,
            ai_turn_write_id: None,
            last_ai_turn_post: None,
            ai_turn_backoff: AI_TURN_POST_MIN_INTERVAL,
            status_busy: false,
            status_queued: 0,
            pending_member_prompts: VecDeque::new(),
            first_posted_turn: None,
            anchor_in_flight: None,
            start_failed_at: None,
            pending_final_writes: BTreeSet::new(),
            final_writes_pending_revoke: false,
            pending_final_turns: BTreeSet::new(),
            final_writes_deadline: None,
            retire_after_final_writes: false,
        }
    }

    /// Discard stopped work so a later activation cannot publish its snapshots.
    fn discard_session_work(&mut self) {
        self.posted_by_tool_this_turn = false;
        self.pending_tool_calls.clear();
        self.active_turn = None;
        self.pending_ai_turns.clear();
        self.ai_turn_in_flight = false;
        self.ai_turn_write_id = None;
        self.anchor_in_flight = None;
        self.status_busy = false;
        self.status_queued = 0;
        self.pending_member_prompts.clear();
        self.pending_final_writes.clear();
        self.final_writes_pending_revoke = false;
        self.pending_final_turns.clear();
        self.final_writes_deadline = None;
        self.retire_after_final_writes = false;
    }
}

/// A completed build waiting for the user's decision on these captured bytes.
struct PendingGeneratedApp {
    manifest: Box<MiniAppManifest>,
    refine_of: Option<MiniAppId>,
    context: a2app_core::information_flow::ContextId,
    epoch: u64,
    request: String,
    attribution: Option<(Option<VersionActor>, AcquisitionSource)>,
    action_target: Option<String>,
    review_id: Option<u64>,
    effect_review_id: Option<u64>,
}

impl PendingGeneratedApp {
    fn matches_review(&self, review: &a2app_core::information_flow::EffectReview) -> bool {
        self.effect_review_id == Some(review.id) && self.context == review.context && self.epoch == review.epoch
            && matches!((serde_json::to_value(&self.manifest), serde_json::from_str::<serde_json::Value>(&review.payload)),
                (Ok(payload), Ok(captured)) if payload == captured)
    }

    fn action(&self) -> Option<a2app_core::information_flow::SensitiveAction> {
        self.action_target.as_ref().map(|target| a2app_core::information_flow::SensitiveAction {
            kind: "apps.generate".into(), target: target.clone(),
        })
    }

    fn install_action(&self) -> a2app_core::information_flow::SensitiveAction {
        self.action().unwrap_or_else(|| a2app_core::information_flow::SensitiveAction {
            kind: "apps.generate".into(), target: self.manifest.id.clone(),
        })
    }

    #[cfg(test)]
    fn commit_review(&self, commit: impl FnOnce(&a2app_core::information_flow::ContextId, u64,
        &a2app_core::information_flow::SensitiveAction, &serde_json::Value) -> Result<(), String>) -> Result<(), String>
    {
        let Some(action) = self.action() else { return Ok(()) };
        let payload = serde_json::to_value(&self.manifest).map_err(|_| "Cannot review generated mini-app content.")?;
        commit(&self.context, self.epoch, &action, &payload)
    }

    fn can_retain(&self) -> bool {
        struct Size(usize);
        impl std::io::Write for Size {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self.0.checked_add(bytes.len()).filter(|size| *size <= 1024 * 1024)
                    .ok_or_else(|| std::io::Error::other("Completed build retention limit exceeded"))?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        serde_json::to_writer(Size(0), &self.manifest).is_ok()
    }
}

impl Drop for PendingGeneratedApp {
    fn drop(&mut self) {
        if let Some(id) = self.review_id { let _ = a2app_core::information_flow::cancel_exact_action(id); }
        if let Some(id) = self.effect_review_id { let _ = a2app_core::information_flow::cancel_effect(id); }
    }
}

/// All a2app state, owned by the UI thread.
pub struct A2AppState {
    pub registry: AppRegistry,
    /// Local imports keyed by account, room, and event ID or media URI.
    imported_room_apps: HashMap<(String, String, String), MiniAppId>,
    room_imports: HashMap<String, u64>,
    next_room_import: u64,
    pub permissions: PermissionStore,
    pub persisted: A2AppPersistedState,
    pub broker: Broker,
    pub prompts: VecDeque<PermissionPrompt>,
    pub active_prompt: Option<PermissionPrompt>,
    active_prompt_batch: Vec<PermissionPrompt>,
    permission_batch_busy: bool,
    permission_setups: HashMap<(usize, u64), permission_batch::PermissionSetup>,
    permission_gestures: BTreeMap<a2app_core::information_flow::ContextId, (u64, Instant)>,
    dismissed_effects: BTreeSet<(a2app_core::information_flow::ContextId, String)>,
    /// Upfront task-permission prompts, in their own queue and shown ahead of
    /// single-permission prompts, one at a time.
    #[cfg(unix)]
    pub task_prompts: VecDeque<TaskPrompt>,
    #[cfg(unix)]
    pub active_task: Option<TaskPrompt>,
    /// Parked exact-action reviews, in their own queue and shown ahead of
    /// task prompts, one at a time.
    #[cfg(unix)]
    pub exact_reviews: VecDeque<ExactReviewPrompt>,
    #[cfg(unix)]
    pub active_exact_review: Option<ExactReviewPrompt>,
    /// Exactly what each live task applied, so the turn's end, session
    /// teardown and the AI panel's Revoke can drop it.
    #[cfg(unix)]
    pub task_ledger: TaskLedger,
    /// Task ids are unique for the process; the tool echoes one back.
    #[cfg(unix)]
    next_task_id: u64,
    /// The user's per-room "Not now" memory for task plans, keyed by
    /// `(room id, needs fingerprint)`: the exact set of need keys dismissed.
    /// Cleared when the turn closes, so a later request can ask again.
    #[cfg(unix)]
    dismissed_task_plans: HashMap<(String, [u8; 32]), BTreeSet<String>>,
    /// How many `request_task_permissions` calls a room has made this turn.
    /// Capped so a looping agent cannot re-ask forever; cleared on turn close.
    #[cfg(unix)]
    task_request_counts: HashMap<String, u32>,
    /// (app, permission) pairs the user said "Not Now" to this session.
    /// The key is the subject: an app id or an AI room's agent key.
    pub dismissed_prompts: HashSet<(String, Permission)>,
    /// (subject, host) pairs the user said "Not Now" to this session for the
    /// agent's internet tools. Kept separate from `dismissed_prompts` because
    /// network prompts are per HOST: dismissing one host must not silently
    /// refuse every other host for the rest of the session.
    pub dismissed_net_hosts: HashSet<(String, String)>,
    pub generation: Option<Generation>,
    pending_generated: Option<PendingGeneratedApp>,
    pub generation_context: Option<a2app_core::information_flow::ContextId>,
    generation_attribution: Option<(Option<VersionActor>, AcquisitionSource)>,
    generation_epoch: Option<u64>,
    pub console: GenConsole,
    /// The request text of a failed generation, offered for Retry.
    pub failed_request: Option<String>,
    pub agent_prefs: AgentPrefs,
    /// The room the next created app will be scoped to, if any.
    pub create_room: Option<OwnedRoomId>,
    /// The app whose host pane is currently shown, gating UI-class services.
    pub foreground_app: Option<MiniAppId>,
    /// A mini-app's request of a room's RoomScreen, waiting to be taken by
    /// the screen showing that room (see [`take_room_action`]).
    room_action: Option<PendingRoomAction>,
    /// Live incoming-hook subscriptions, by isolate heap key.
    hook_subs: HashMap<usize, HookSubscription>,
    /// Rooms the worker is watching for those subscriptions.
    watched_rooms: HashSet<OwnedRoomId>,
    /// Whether the worker runs the account watch for them too.
    account_watched: bool,
    /// One long-lived AI agent session per AI room. Sessions bind their own
    /// tool server, spawn their agent, and answer tool calls drained here
    /// each event pass.
    #[cfg(unix)]
    pub ai_sessions: HashMap<OwnedRoomId, AiSession>,
    /// Rooms known to carry the `rs.robius.robrix.ai_room` marker, and their
    /// forwarding progress. Populated by [`on_room_shown`] the first time a
    /// room is opened.
    #[cfg(unix)]
    pub ai_rooms: HashMap<OwnedRoomId, AiRoomInfo>,
    /// Rooms already checked and found to have no marker, so re-opening the
    /// same ordinary room doesn't re-check it every time.
    #[cfg(unix)]
    known_non_ai_rooms: HashSet<OwnedRoomId>,
    /// The room whose session launched the current generation, while one is
    /// running for a `launch_splash_app` tool call. `None` when the current
    /// (or last-finished) generation came from the Mini Apps screen instead —
    /// that is what lets completion answer the waiting tool call and run the
    /// finished app in the right room.
    #[cfg(unix)]
    pub ai_generation_room: Option<OwnedRoomId>,
    /// Granted attached-room reads in flight, keyed by request id: the room
    /// and tool they belong to, plus the channel that must answer the waiting
    /// tool call when the async worker's [`AiRoomAction::ToolReadResult`]
    /// lands (the tool is kept so the receipt chip can name it).
    #[cfg(unix)]
    pub ai_reads: HashMap<u64, AiReadPending>,
    /// Cross-room `post_room_message` posts in flight, keyed by request id:
    /// the session room they belong to (for the receipt) plus the channel
    /// that must answer the waiting tool call when the async worker's
    /// [`AiRoomAction::PostToRoomResult`] lands.
    #[cfg(unix)]
    pub ai_posts: HashMap<u64, AiPostPending>,
    #[cfg(unix)]
    ai_fetches: HashMap<u64, (OwnedRoomId, Sender<Result<String, String>>, std::sync::Arc<std::sync::atomic::AtomicBool>)>,
    /// `send_message` writes in flight, keyed by request id; see
    /// [`AiReplyPending`].
    #[cfg(unix)]
    ai_replies: HashMap<u64, AiReplyPending>,
    /// "Allow Once" answers for cross-room posts/reads: (agent subject,
    /// group, room id). Session-only; dropped with the room's session.
    #[cfg(unix)]
    once_rooms: HashSet<(String, Permission, String)>,
    /// Live mini-app tools on each room's agent session, by the namespaced
    /// name the model sees. What routes an incoming `tools/call` back to the
    /// owning isolate, and what teardown prunes.
    #[cfg(unix)]
    pub app_tools: HashMap<String, AppToolRegistration>,
    /// App-tool invocations waiting on the app's answer, by call id.
    #[cfg(unix)]
    pub app_tool_calls: HashMap<u64, AppToolPending>,
    perms_dirty: bool,
    registry_dirty: bool,
    last_persist: Instant,
    last_timed_check: Instant,
    policy_spaces_pending: bool,
    policy_spaces_ready: bool,
    pub policy_spaces_status: Option<String>,
    policy_spaces_last: Option<Instant>,
    policy_spaces_revision: u64,
    policy_space_roots: std::collections::BTreeSet<String>,
    /// A slow heartbeat kept alive only while a generation runs, so the
    /// stall watchdog in [`advance_generation`] fires even when a hung agent
    /// stops producing events (see [`crate::a2app_agent::pipeline`]'s
    /// `STALL_SECS`). Stopped once no generation is live.
    generation_timer: Option<Timer>,
    /// Timer that wakes the runtime to flush coalesced `ai_turn` state-event
    /// writes while any room still has a pending snapshot or a write in
    /// flight.
    #[cfg(unix)]
    ai_turn_flush_timer: Option<Timer>,
}

impl A2AppState {
    /// Marks permission state dirty; saved (throttled) at the end of `process`.
    pub fn mark_perms_dirty(&mut self) {
        self.perms_dirty = true;
    }

    pub fn is_running(&self, app_id: &str) -> bool {
        instances::is_running(app_id)
    }

    fn clear_adopted_builtin_update(&mut self, manifest: &MiniAppManifest) {
        if manifest.builtin && builtin::stock(&manifest.id).is_some_and(|stock| builtin::matches_default(manifest, &stock)) {
            self.persisted.builtin_updates.remove(&manifest.id);
            self.registry_dirty = true;
        }
    }
}

/// One-time startup: loads all persisted a2app state into the thread-local.
pub fn init() {
    a2app_core::set_data_root(crate::app_data_dir().join("a2app"));
    if let Err(error) = a2app_core::information_flow::init(a2app_core::data_root()) {
        error!("Mini-app information-flow protection: {error}");
    }
    if let Err(error) = a2app_core::information_flow::set_effect_room_scope_matcher(matrix::policy::effect_room_scope_matches) {
        error!("Mini-app room permission matching: {error}");
    }

    let mut registry = AppRegistry::new(builtin::builtin_apps());
    for app in persistence::load_user_apps() {
        registry.insert(app);
    }
    let mut persisted = persistence::load_registry_state();
    let builtin_ids: Vec<MiniAppId> = registry.iter().filter(|app| app.builtin).map(|app| app.id.clone()).collect();
    for app_id in builtin_ids {
        let Some(current) = registry.get(&app_id).cloned() else { continue };
        let Some(stock) = builtin::stock(&app_id) else { continue };
        match builtin::reconcile_builtin_with_host(
            &current, &stock, persisted.builtin_baselines.get(&app_id),
            persisted.builtin_updates.get(&app_id).map(String::as_str), versions::now_unix(), utc_offset_secs(),
            Some(env!("CARGO_PKG_VERSION")), Some(env!("ROBRIX_GIT_COMMIT_HASH")),
        ) {
            Ok(update) => {
                if update.adopted_default || current.current_version != update.manifest.current_version
                    || !persistence::has_app_files(&app_id)
                {
                    if let Err(error) = persistence::save_user_app(&update.manifest) {
                        error!("Could not save the bundled mini-app update for {app_id}: {error}");
                        continue;
                    }
                }
                persisted.builtin_baselines.insert(app_id.clone(), update.baseline);
                match update.available_update {
                    Some(stamp) => { persisted.builtin_updates.insert(app_id.clone(), stamp); }
                    None => { persisted.builtin_updates.remove(&app_id); }
                }
                registry.insert(update.manifest);
            }
            Err(error) => error!("Could not record the bundled mini-app update for {app_id}: {error}"),
        }
    }
    if let Err(error) = persistence::save_registry_state(&persisted) {
        error!("Could not save bundled mini-app update tracking: {error}");
    }
    // Known bundled code and an empty, stopped sandbox can start fresh after
    // the old launch/input tracking falsely marked them private. History and
    // sandboxes containing saved data keep their original protection.
    for manifest in registry.iter().filter(|manifest| !super::information_flow::manifest_has_private_source(manifest)) {
        if let Err(error) = super::information_flow::reconcile_builtin_manifest(manifest) {
            error!("Could not restore bundled mini-app provenance for {}: {error}", manifest.id);
        }
    }
    let permissions = persistence::load_permissions();
    a2app_core::permissions::publish_snapshot(permissions.snapshot(&registry));
    crate::a2app::matrix::publish_permission_policy(&permissions);

    initialize_state(registry, permissions, persisted, a2app_agent::prefs::load_agent_prefs());
    super::background::init();
}

fn initialize_state(registry: AppRegistry, permissions: PermissionStore, persisted: A2AppPersistedState, agent_prefs: a2app_agent::prefs::AgentPrefs) {
    A2APP.with(|state| {
        *state.borrow_mut() = Some(A2AppState {
            imported_room_apps: index_room_imports(&registry),
            room_imports: HashMap::new(),
            next_room_import: 0,
            registry,
            permissions,
            persisted,
            broker: Broker::new(),
            prompts: VecDeque::new(),
            active_prompt: None,
            active_prompt_batch: Vec::new(),
            permission_batch_busy: false,
            permission_setups: HashMap::new(),
            permission_gestures: BTreeMap::new(),
            dismissed_effects: BTreeSet::new(),
            #[cfg(unix)]
            task_prompts: VecDeque::new(),
            #[cfg(unix)]
            active_task: None,
            #[cfg(unix)]
            exact_reviews: VecDeque::new(),
            #[cfg(unix)]
            active_exact_review: None,
            #[cfg(unix)]
            task_ledger: TaskLedger::default(),
            #[cfg(unix)]
            next_task_id: 0,
            #[cfg(unix)]
            dismissed_task_plans: HashMap::new(),
            #[cfg(unix)]
            task_request_counts: HashMap::new(),
            dismissed_prompts: HashSet::new(),
            dismissed_net_hosts: HashSet::new(),
            generation: None,
            pending_generated: None,
            generation_context: None,
            generation_attribution: None,
            generation_epoch: None,
            console: GenConsole::default(),
            failed_request: None,
            agent_prefs,
            create_room: None,
            foreground_app: None,
            room_action: None,
            hook_subs: HashMap::new(),
            watched_rooms: HashSet::new(),
            account_watched: false,
            #[cfg(unix)]
            ai_sessions: HashMap::new(),
            #[cfg(unix)]
            ai_rooms: HashMap::new(),
            #[cfg(unix)]
            known_non_ai_rooms: HashSet::new(),
            #[cfg(unix)]
            ai_generation_room: None,
            #[cfg(unix)]
            ai_reads: HashMap::new(),
            #[cfg(unix)]
            ai_posts: HashMap::new(),
            #[cfg(unix)]
            ai_fetches: HashMap::new(),
            #[cfg(unix)]
            ai_replies: HashMap::new(),
            #[cfg(unix)]
            once_rooms: HashSet::new(),
            #[cfg(unix)]
            app_tools: HashMap::new(),
            #[cfg(unix)]
            app_tool_calls: HashMap::new(),
            perms_dirty: false,
            registry_dirty: false,
            last_persist: Instant::now(),
            last_timed_check: Instant::now(),
            policy_spaces_pending: false,
            policy_spaces_ready: false,
            policy_spaces_status: None,
            policy_spaces_last: None,
            policy_spaces_revision: matrix::spaces::policy_spaces_revision(),
            policy_space_roots: Default::default(),
            generation_timer: None,
            #[cfg(unix)]
            ai_turn_flush_timer: None,
        });
    });
}

#[cfg(test)]
pub(super) fn initialize_background_test(manifest: MiniAppManifest) {
    initialize_state(AppRegistry::new(vec![manifest]), PermissionStore::default(), A2AppPersistedState::default(), Default::default());
}

#[cfg(test)]
pub(super) fn process_background_test_broker(cx: &mut Cx) { process_broker(cx, &WidgetRef::empty()); }

/// Operations that a2app widgets request; applied centrally in [`process`].
#[derive(Clone, Debug)]
pub enum A2AppOp {
    /// Opens (or brings back) an app. `room_id` attaches a room context
    /// (`None` falls back to the app's own scope); `in_room_pane` docks it
    /// into that room's RoomScreen pane instead of the generic host modal.
    OpenApp { app_id: MiniAppId, room_id: Option<OwnedRoomId>, in_room_pane: bool },
    OpenPublicApp(MiniAppId),
    ReviewFlow(a2app_core::information_flow::ContextId),
    OpenAppFromContext { context: a2app_core::information_flow::ContextId, flow_epoch: u64, app_id: MiniAppId, room_id: OwnedRoomId },
    CloseHostPane,
    ForceStop(MiniAppId),
    Uninstall(MiniAppId),
    ClearData(MiniAppId),
    SaveBackgroundTask { binding: a2app_core::background::JobBinding, trigger: a2app_core::background::Trigger, expected_fingerprint: String },
    SetBackgroundTaskEnabled { id: u64, enabled: bool, expected_fingerprint: String },
    RunBackgroundTask(u64),
    RemoveBackgroundTask(u64),
    Export(MiniAppId),
    ImportText(String),
    ImportRoomBundle { text: String, room_id: Option<OwnedRoomId>, event_id: Option<OwnedEventId>, sender_id: String, sender_name: String, shared_at_unix: Option<u64> },
    ImportRoomAttachment {
        media_source: matrix_sdk::ruma::events::room::MediaSource,
        filename: String,
        size: Option<u64>,
        room_id: OwnedRoomId,
        event_id: Option<OwnedEventId>,
        sender_id: String,
        sender_name: String,
        shared_at_unix: Option<u64>,
    },
    RoomAttachmentDownloaded {
        request_id: u64,
        media_uri: String,
        account: String,
        actor: Option<VersionActor>,
        source: AcquisitionSource,
        text: Result<String, String>,
    },
    ImportFile(PathBuf),
    /// Makes an archived version the working copy; every other version stays.
    SwitchVersion { app_id: MiniAppId, stamp: String },
    /// Installs hand-edited source as a new version.
    SaveSource { app_id: MiniAppId, source: String },
    /// Puts a built-in back on its stock source, as a version like any other.
    ResetToStock(MiniAppId),
    /// Hands the app's bundle file to the system share sheet.
    ShareBundle(MiniAppId),
    /// Jumps to `room_id` with the app's bundle staged as a file upload.
    SendToRoom { app_id: MiniAppId, room_id: OwnedRoomId },
    /// Jumps to `room_id` and docks the app there.
    OpenInRoom { app_id: MiniAppId, room_id: OwnedRoomId },
    SetPermission { app_id: MiniAppId, perm: Permission, state: GrantState },
    /// Resets all mini-app and agent permissions and room protection settings.
    ResetAllPermissions,
    ResetAppPermissions(MiniAppId),
    /// A single ability's own answer under its group (`Ask` = follow group).
    SetCapability { app_id: MiniAppId, cap_id: String, state: GrantState },
    GrantScoped { app_id: MiniAppId, perm: Permission, cap_id: Option<String>, scope: RoomScope, duration: GrantDuration, origin_room: Option<String> },
    GrantNetwork { app_id: MiniAppId, network: NetworkScope, scope: RoomScope, duration: GrantDuration, origin_room: Option<String> },
    RevokeScopedGrant(u64),
    RevokeNetworkGrant(u64),
    ClearLegacyGrants { subject: String, perm: Permission },
    SetGlobalPolicy { access: RoomAccess, decision: PolicyDecision },
    SetPolicyMode { access: RoomAccess, mode: RoomPolicyMode },
    SetMatrixWrite(bool),
    SetRoomPolicy { room_id: String, access: RoomAccess, decision: PolicyDecision },
    SetSpacePolicy { space_id: String, access: RoomAccess, decision: PolicyDecision },
    SetFlowPolicy { source: a2app_core::information_flow::Source, policy: a2app_core::information_flow::FlowPolicy },
    GrantFlowSharing {
        source: a2app_core::information_flow::Source,
        recipient: a2app_core::information_flow::Recipient,
        reader: a2app_core::information_flow::ReaderScope,
        duration: a2app_core::information_flow::SharingDuration,
    },
    RevokeFlowSharing(u64),
    GrantFlowAuthority {
        context: a2app_core::information_flow::ContextId,
        action: a2app_core::information_flow::SensitiveAction,
        session: a2app_core::information_flow::AuthoritySession,
        expected_influences: a2app_core::information_flow::Influences,
        expected_epoch: u64,
    },
    RevokeFlowAuthority(u64),
    RevokeEffectAuthority(u64),
    GrantExactFlowAuthority {
        context: a2app_core::information_flow::ContextId,
        request_id: u64,
        expected_influences: a2app_core::information_flow::Influences,
        expected_epoch: u64,
    },
    RoomClosed(OwnedRoomId),
    Unrestrict(MiniAppId),
    /// Starts a generation; `Modify` intent is classified from the text.
    StartGeneration { request: String, room_id: Option<OwnedRoomId> },
    /// Starts a generation that must create a new app, whatever the text
    /// looks like.
    StartCreate { request: String, room_id: Option<OwnedRoomId> },
    StartModify { app_id: MiniAppId, request: String },
    CancelGeneration,
    RetryGeneration,
    /// Clears the finished console back to the composer.
    NewPrompt,
    /// Posts an app's bundle into a room as a custom event.
    ShareToRoom { app_id: MiniAppId, room_id: OwnedRoomId },
    /// Opens the "AI in this room" management panel for an AI room.
    #[cfg(unix)]
    AiRoomPanel(OwnedRoomId),
    /// The user pressed Escape in an AI room: abort whatever its agent is
    /// currently doing (an in-flight turn, queued prompts, or the app build
    /// its `launch_splash_app` tool is waiting on), leaving the session idle.
    #[cfg(unix)]
    AbortAiRoom(OwnedRoomId),
}


/// One isolate's live hooks; `room_id` is its attached room, which the
/// account-wide hooks don't need.
struct HookSubscription {
    app_id: MiniAppId,
    room_id: Option<OwnedRoomId>,
    hooks: HashSet<&'static str>,
}

/// What a mini-app (or the Mini Apps screen) asked the RoomScreen of its
/// room to do.
#[derive(Clone, Debug)]
pub enum RoomAction {
    ShowUserProfile(OwnedUserId),
    JumpToEvent(OwnedEventId),
    ReplyTo(OwnedEventId),
    InsertDraft(String),
    /// Pre-fills the room's file upload with a file Robrix wrote.
    StageAttachment { path: PathBuf, caption: String },
    /// Docks the app into the room's pane.
    OpenApp(MiniAppId),
}

/// Runtime-side changes the Mini Apps screen re-reads on.
#[derive(Clone, Debug, Default)]
pub enum A2AppRuntimeAction {
    VersionsChanged(MiniAppId),
    Uninstalled(MiniAppId),
    ReviewFlow(a2app_core::information_flow::ContextId),
    #[default]
    None,
}

#[derive(Clone, Debug)]
struct HostNetworkResult {
    reply: Reply,
    context: a2app_core::information_flow::ContextId,
    flow_epoch: u64,
    result: Result<String, String>,
}

#[cfg(unix)]
#[derive(Clone, Debug)]
struct AgentNetworkResult {
    id: u64,
    context: a2app_core::information_flow::ContextId,
    result: Result<String, String>,
}

struct PendingRoomAction {
    room_id: OwnedRoomId,
    action: RoomAction,
    since: Instant,
    authorization: Option<matrix::policy::MatrixAuthorization>,
    close_after: Option<instances::InstanceKey>,
}

impl PendingRoomAction {
    /// Replacing a request from the same activation keeps its deferred
    /// teardown attached to the new request.
    fn inherit_requester(&mut self, previous: &mut Self) {
        let (Some(current), Some(old)) = (&self.authorization, &previous.authorization) else { return };
        if current.flow_context.is_some() && current.flow_epoch.is_some()
            && current.flow_context == old.flow_context && current.flow_epoch == old.flow_epoch
        {
            self.close_after = previous.close_after.take();
        }
    }

    fn permitted(&self, permissions: &PermissionStore) -> bool {
        self.authorization.as_ref().is_none_or(|authorization| authorization.permits(permissions, Some(self.room_id.as_str())))
    }

    fn check(&self) -> Result<(), String> {
        if self.since.elapsed() > ROOM_ACTION_TTL {
            return Err("The queued room action expired.".into());
        }
        if let Some(authorization) = &self.authorization {
            let permitted = with_a2app(|state| self.permitted(&state.permissions)).unwrap_or(false);
            if !permitted { return Err("Permission for the queued room action was revoked.".into()); }
            authorization.check_flow(Some(self.room_id.as_str()), RoomAccess::Write)?;
            let payload = match &self.action {
                RoomAction::InsertDraft(text) => serde_json::json!({ "text": text }),
                RoomAction::ReplyTo(event_id) => serde_json::json!({ "event_id": event_id }),
                _ => return Err("Unsupported protected composer action.".into()),
            };
            authorization.commit_action(self.room_id.as_str(), &payload)?;
        }
        Ok(())
    }

    /// Retire a modal requester after consumption or cancellation, without
    /// touching a replacement instance or one the user has shown again.
    fn close_requester(&self, cx: &mut Cx) {
        let Some(key) = &self.close_after else { return };
        let Some(authorization) = &self.authorization else { return };
        let Some(context) = &authorization.flow_context else { return };
        let Some(epoch) = authorization.flow_epoch else { return };
        if instances::context_of_key(key).as_ref() == Some(context)
            && instances::surface_of(key).is_none()
            && a2app_core::information_flow::ensure_context_epoch(context, epoch).is_ok()
            && instances::quit(cx, key)
        {
            app_stopped(cx, &key.0);
        }
    }
}

/// Pokes the RoomScreen showing `room_id` to take its pending [`RoomAction`].
#[derive(Clone, Debug, Default)]
pub enum A2AppRoomAction {
    Pending { room_id: OwnedRoomId },
    #[default]
    None,
}

/// Hands the pending action for `room_id` to the one RoomScreen that asks
/// first; a stale one (its room never opened) is dropped instead.
pub fn take_room_action(cx: &mut Cx, room_id: &RoomId) -> Option<RoomAction> {
    let pending = with_a2app(|state| {
        let pending = state.room_action.as_ref()?;
        if pending.room_id != room_id {
            return None;
        }
        state.room_action.take()
    }).flatten()?;
    let result = pending.check();
    pending.close_requester(cx);
    match result {
        Ok(()) => Some(pending.action),
        Err(error) => {
            enqueue_popup_notification(error, PopupKind::Warning, Some(6.0));
            None
        }
    }
}



/// Drives all a2app machinery for one event pass. Called from
/// `App::handle_event` on every event; cheap early-outs keep it off the
/// hot path for events it doesn't care about.
pub fn process(cx: &mut Cx, ui: &WidgetRef, event: &Event) {
    match event {
        Event::Background | Event::Pause | Event::Shutdown => { super::background::lifecycle(cx, false); return; }
        Event::Foreground | Event::Resume => { super::background::lifecycle(cx, true); super::background::process(cx, &Event::Signal); return; }
        _ => {}
    }
    let expired = with_a2app(|state| state.room_action.take_if(|pending| pending.since.elapsed() > ROOM_ACTION_TTL)).flatten();
    if let Some(pending) = expired { pending.close_requester(cx); }
    if let Event::NetworkResponses(e) = event {
        with_a2app(|state| state.broker.handle_network(cx, e, &super::information_flow::check_response));
        instances::handle_network_responses(cx, event, &mut Scope::empty());
        return;
    }
    match event {
        Event::Signal | Event::Actions(_) | Event::Timer(_) => {}
        _ => return,
    }
    instances::flush_pending(cx);

    let mut ops: Vec<A2AppOp> = Vec::new();
    let mut prompt_answers: Vec<PermissionPromptResponse> = Vec::new();
    let mut group_answers: Vec<PermissionPromptGroupResponse> = Vec::new();
    #[cfg(unix)]
    let mut task_answers: Vec<TaskPermissionAction> = Vec::new();
    let mut matrix_results: Vec<A2AppMatrixResult> = Vec::new();
    let mut network_results: Vec<HostNetworkResult> = Vec::new();
    let mut pane_actions: Vec<MiniAppHostPaneAction> = Vec::new();
    let mut stopped: Vec<MiniAppId> = Vec::new();
    let mut watch_events: Vec<A2AppRoomWatchEvent> = Vec::new();
    let mut account_events: Vec<A2AppAccountWatchEvent> = Vec::new();
    let mut host_events: Vec<(&'static str, serde_json::Value)> = Vec::new();
    #[cfg(unix)]
    let mut ai_room_actions: Vec<AiRoomAction> = Vec::new();
    #[cfg(unix)]
    let mut agent_network_results: Vec<AgentNetworkResult> = Vec::new();
    #[cfg(unix)]
    let mut ai_panel_actions: Vec<AiRoomPanelAction> = Vec::new();
    if let Event::Actions(actions) = event {
        for action in actions {
            if matches!(action.downcast_ref(), Some(crate::logout::logout_confirm_modal::LogoutAction::ClearAppState { .. })) {
                with_a2app(|state| state.room_imports.clear());
                super::background::suspend(cx, true);
                super::room_watch::stop_all();
                with_a2app(|state| state.watched_rooms.clear());
                let _ = a2app_core::information_flow::end_session();
                stop_private_contexts(cx, ui);
                matrix::spaces::stop_policy_space_watch();
                invalidate_policy_spaces(cx);
            }
            if matches!(action.downcast_ref(), Some(crate::login::login_screen::LoginAction::LoginSuccess)) {
                with_a2app(|state| state.room_imports.clear());
                super::background::suspend(cx, false);
                super::room_watch::stop_all();
                with_a2app(|state| state.watched_rooms.clear());
                let _ = a2app_core::information_flow::end_session();
                stop_private_contexts(cx, ui);
                matrix::spaces::stop_policy_space_watch();
                with_a2app(|state| state.policy_spaces_pending = false);
                invalidate_policy_spaces(cx);
            }
            if action.downcast_ref::<matrix::spaces::A2AppPolicySpacesInvalidated>().is_some() {
                invalidate_policy_spaces(cx);
                refresh_permission_scope_targets(cx, ui);
                continue;
            }
            if let Some(update) = action.downcast_ref::<matrix::spaces::A2AppPolicySpaces>() {
                let current = matrix::spaces::policy_spaces_revision();
                with_a2app(|state| {
                    state.policy_spaces_pending = false;
                    state.policy_spaces_ready = update.error.is_none() && update.revision == current;
                    state.policy_spaces_status = update.error.clone();
                    state.permissions.clear_room_spaces();
                    if update.error.is_none() && update.revision == current {
                        for (room, spaces) in &update.rooms {
                            state.permissions.set_room_spaces(room, spaces.clone());
                        }
                        state.policy_spaces_revision = current;
                    } else if update.revision != current {
                        state.policy_spaces_last = None;
                    }
                });
                if let Some(error) = &update.error { warning!("Mini-app space permissions: {error}"); }
                publish_grants(cx);
                refresh_permission_scope_targets(cx, ui);
                ui.redraw(cx);
                continue;
            }
            if let Some(watch_event) = action.downcast_ref::<A2AppRoomWatchEvent>() {
                if watch_event.is_current() { watch_events.push(watch_event.clone()); }
                continue;
            }
            if let Some(watch_event) = action.downcast_ref::<A2AppAccountWatchEvent>() {
                account_events.push(watch_event.clone());
                continue;
            }
            if let Some(op) = action.downcast_ref::<A2AppOp>() {
                ops.push(op.clone());
                continue;
            }
            if let Some(answer) = action.downcast_ref::<PermissionPromptGroupResponse>() {
                group_answers.push(answer.clone());
                continue;
            }
            if let Some(answer) = action.downcast_ref::<PermissionPromptResponse>() {
                prompt_answers.push(answer.clone());
                continue;
            }
            #[cfg(unix)]
            if let Some(answer) = action.downcast_ref::<TaskPermissionAction>() {
                task_answers.push(answer.clone());
                continue;
            }
            if let Some(result) = action.downcast_ref::<A2AppMatrixResult>() {
                matrix_results.push(result.clone());
                continue;
            }
            if let Some(result) = action.downcast_ref::<HostNetworkResult>() {
                network_results.push(result.clone());
                continue;
            }
            if let Some(pane_action) = action.downcast_ref::<MiniAppHostPaneAction>() {
                pane_actions.push(pane_action.clone());
                continue;
            }
            if let Some(MiniAppInstanceAction::AppStopped(app_id)) = action.downcast_ref() {
                stopped.push(app_id.clone());
                continue;
            }
            if let Some(AppStateAction::RoomFocused(selected)) = action.downcast_ref() {
                let room = |r: &RoomNameId| (r.room_id().to_string(), r.display_name().to_string());
                let payload = match selected {
                    SelectedRoom::JoinedRoom { room_name_id } => {
                        let (room_id, name) = room(room_name_id);
                        serde_json::json!({ "kind": "room", "room_id": room_id, "name": name })
                    }
                    SelectedRoom::Thread { room_name_id, thread_root_event_id } => {
                        let (room_id, name) = room(room_name_id);
                        serde_json::json!({ "kind": "thread", "room_id": room_id, "name": name, "thread_root": thread_root_event_id })
                    }
                    SelectedRoom::InvitedRoom { room_name_id } => {
                        let (room_id, name) = room(room_name_id);
                        serde_json::json!({ "kind": "invite", "room_id": room_id, "name": name })
                    }
                    // A popped-out pane shows info about its room.
                    SelectedRoom::RoomPane { room_name_id, .. } => {
                        let (room_id, name) = room(room_name_id);
                        serde_json::json!({ "kind": "room", "room_id": room_id, "name": name })
                    }
                    SelectedRoom::Space { .. } => serde_json::json!({ "kind": "space" }),
                };
                host_events.push(("on_active_room_changed", payload));
                continue;
            }
            if let Some(nav) = action.downcast_ref::<NavigationBarAction>() {
                let (screen, space_id) = match nav {
                    NavigationBarAction::GoToHome => ("home", None),
                    NavigationBarAction::GoToAddRoom { .. } => ("add_room", None),
                    NavigationBarAction::GoToMiniApps => ("mini_apps", None),
                    NavigationBarAction::OpenSettings => ("settings", None),
                    NavigationBarAction::GoToSpace { space_name_id } => ("space", Some(space_name_id.room_id().to_string())),
                    _ => continue,
                };
                host_events.push(("on_navigation_changed", serde_json::json!({ "screen": screen, "space_id": space_id })));
                continue;
            }
            if action.downcast_ref::<AppPreferencesAction>().is_some() {
                host_events.push(("on_prefs_changed", prefs_json(cx)));
            }
            #[cfg(unix)]
            if let Some(result) = action.downcast_ref::<AgentNetworkResult>() {
                agent_network_results.push(result.clone());
                continue;
            }
            #[cfg(unix)]
            if let Some(ai_room_action) = action.downcast_ref::<AiRoomAction>() {
                ai_room_actions.push(ai_room_action.clone());
            }
            #[cfg(unix)]
            if let Some(panel_action) = action.downcast_ref::<AiRoomPanelAction>() {
                ai_panel_actions.push(panel_action.clone());
            }
        }
    }

    if with_a2app(|state| state.policy_spaces_revision != matrix::spaces::policy_spaces_revision()).unwrap_or(false) {
        invalidate_policy_spaces(cx);
        refresh_permission_scope_targets(cx, ui);
    }
    request_policy_spaces();
    // Deliver finished matrix calls back into their isolates. The callback
    // typically updates the app's UI, so schedule a repaint.
    let any_results = !matrix_results.is_empty() || !network_results.is_empty();
    for op in ops.drain(..) { apply_op(cx, ui, op); }
    for result in matrix_results {
        let reply = result.reply;
        let requested_context = result.authorization.as_ref().and_then(|auth| auth.flow_context.clone());
        let capability = result.authorization.as_ref().map(|auth| auth.capability.clone());
        let activation_check = result.authorization.as_ref().ok_or("Missing Matrix authorization.".to_string())
            .and_then(|authorization| authorization.check_context());
        let result = with_a2app(|state| result.checked_result(&state.permissions))
            .unwrap_or_else(|| Err("Permissions are unavailable.".into()))
            .and_then(|data| {
                activation_check?;
                let context = super::information_flow::context_for_heap(reply.heap_key)?;
                if requested_context.as_ref() != Some(&context) {
                    return Err("The requesting Matrix context changed.".into());
                }
                let value = serde_json::from_str(&data).map_err(|_| "Invalid service response.")?;
                super::information_flow::record_matrix_response(&context, capability.as_deref(), &value)?;
                Ok(data)
            });
        match &result {
            Ok(data) => services::respond(cx, reply, Ok(data.as_str())),
            Err(e) => services::respond(cx, reply, Err(e.as_str())),
        }
    }
    #[cfg(unix)]
    for response in agent_network_results {
        if let Some((room, answer, alive)) = with_a2app(|state| state.ai_fetches.remove(&response.id)).flatten() {
            alive.store(false, std::sync::atomic::Ordering::SeqCst);
            let result = super::information_flow::current_context(&response.context).and(response.result);
            note_ai_tool_call(&room, "web_fetch", result.is_ok(), "");
            let _ = answer.send(result);
        }
    }
    for response in network_results {
        // A stopped worker's heap address may be reused. Never answer even an
        // error into a replacement activation's unrelated callback.
        if a2app_core::information_flow::ensure_context_epoch(&response.context, response.flow_epoch).is_err()
            || super::information_flow::context_for_heap(response.reply.heap_key).as_ref() != Ok(&response.context)
        { continue; }
        services::respond(cx, response.reply, response.result.as_deref().map_err(String::as_str));
    }
    if any_results {
        // A callback's ui.X.render() output only commits on the NEXT event
        // pass, so queue one; the NextFrame keeps the paint loop ticking so
        // the commit actually PRESENTS instead of waiting for user input.
        SignalToUI::set_ui_signal();
        let _ = cx.new_next_frame();
        ui.redraw(cx);
    }

    for answer in group_answers.into_iter().take(1) { permission_batch::answer_group(cx, ui, answer); }
    for answer in prompt_answers.into_iter().take(1) {
        sweep_permission_prompts(cx, ui);
        answer_permission_prompt(cx, ui, answer);
    }
    #[cfg(unix)]
    for answer in task_answers {
        answer_task_prompt(cx, ui, answer);
    }
    for pane_action in pane_actions {
        match pane_action {
            MiniAppHostPaneAction::CloseClicked => ops.push(A2AppOp::CloseHostPane),
            // The instance goes back to its room's dock, state intact.
            MiniAppHostPaneAction::ReturnToRoom { app_id, room_id } => {
                host_pane(cx, ui).close_active(cx, true);
                with_a2app(|state| state.foreground_app = None);
                ui.modal(cx, ids!(mini_app_host_modal)).close(cx);
                open_in_room_pane(cx, app_id, room_id);
            }
            MiniAppHostPaneAction::None => {}
        }
    }
    for app_id in stopped {
        app_stopped(cx, &app_id);
    }
    for op in ops {
        apply_op(cx, ui, op);
    }
    if !watch_events.is_empty() || !account_events.is_empty() || !host_events.is_empty() {
        deliver_room_hooks(cx, ui, watch_events, account_events, host_events);
    }
    #[cfg(unix)]
    for action in ai_room_actions {
        apply_ai_room_action(cx, ui, action);
    }
    // The AI room management panel's own answers (applied after the session
    // job/event plumbing, so a power flip or grant lands on a settled state).
    #[cfg(unix)]
    for action in ai_panel_actions {
        apply_ai_room_panel_action(cx, ui, action);
    }

    // Sessions drain their tool calls and agent events every pass; a tool
    // call may start a generation, which advance_generation below then
    // drives to completion (and answers the waiting tool call).
    #[cfg(unix)]
    advance_ai_sessions(cx, ui);
    advance_generation(cx, ui);
    // Keep the generation stall-watchdog heartbeat alive exactly while a
    // generation runs: its own events wake this loop constantly while the
    // agent streams, but a silent (hung) agent produces none, and the stall
    // check in advance_generation can only fire on an event.
    with_a2app(|state| {
        let running = state.generation.is_some();
        match (running, state.generation_timer) {
            (true, None) => {
                state.generation_timer = Some(cx.start_interval(STALL_HEARTBEAT_SECS));
            }
            (false, Some(timer)) => {
                cx.stop_timer(timer);
                state.generation_timer = None;
            }
            _ => {}
        }
    });
    with_a2app(|state| state.permission_batch_busy = true);
    process_broker(cx, ui);
    sweep_permission_prompts(cx, ui);
    for worker in super::effect_review::take_pending() {
        queue_flow_prompt(cx, ui, FlowContinuation::Worker(worker));
    }
    with_a2app(|state| state.permission_batch_busy = false);
    show_next_permission_prompt(cx, ui);

    // An action the user took that was refused or failed gets a popup that
    // says why, on top of the script's own error handling.
    for failure in with_a2app(|state| state.broker.failures()).unwrap_or_default() {
        super::background::record_failure(failure.heap_key, &failure.error);
        if !failure.show_popup || failure.error == "This request was not approved." { continue; }
        let services::ServiceFailure { app_id, error, .. } = failure;
        let name = with_a2app(|state| state.registry.get(&app_id).map(|a| a.name.clone()))
            .flatten()
            .unwrap_or(app_id);
        let mut text = format!("{name}: ");
        let mut chars = error.chars();
        if let Some(first) = chars.next() {
            text.extend(first.to_uppercase());
            text.push_str(chars.as_str());
        }
        if !text.ends_with(['.', '!', '?']) {
            text.push('.');
        }
        enqueue_popup_notification(text, PopupKind::Warning, Some(15.0));
    }
    super::background::finish_failure_drain();
    expire_timed_grants(cx, ui);
    super::background::process(cx, event);
    persist_if_dirty();
}

fn host_pane(cx: &mut Cx, ui: &WidgetRef) -> crate::a2app::host_pane::MiniAppHostPaneRef {
    ui.mini_app_host_pane(cx, ids!(mini_app_host_modal.content))
}

/// One-time grants die with the app's last isolate.
pub(super) fn app_stopped(cx: &mut Cx, app_id: &str) {
    prune_hook_subs();
    if instances::is_running(app_id) {
        return;
    }
    with_a2app(|state| {
        state.permissions.clear_once_for(app_id);
        state.mark_perms_dirty();
        state.foreground_app.take_if(|f| f == app_id);
    });
    publish_grants(cx);
}

/// Shows the app's popped-out pane if it has one,
/// or else docks it in its room's pane, either now or once that room is shown.
fn open_in_room_pane(cx: &mut Cx, app_id: MiniAppId, room_id: OwnedRoomId) {
    let key = (app_id.clone(), Some(room_id.clone()));
    let kind = RoomPaneKind::MiniApp(app_id);
    if instances::surface_of(&key) == Some(Surface::Tab) {
        room_pane::request(cx, room_id, kind, RoomPaneOp::Focus);
        return;
    }
    room_pane::request(cx, room_id.clone(), kind.clone(), RoomPaneOp::Open);
    // If no dock is showing the room, a running app would otherwise only show as a chip there.
    room_pane::dock_when_shown(cx, crate::sliding_sync::TimelineKind::MainRoom { room_id }, kind, None);
}

/// The same context contract applies to picker launches, navigation, and
/// restored room panes. A roomless aggregate app may still open on its own.
fn validate_app_launch_context(manifest: &MiniAppManifest, room_id: Option<&str>, is_space: bool) -> Result<(), String> {
    if let A2AppScope::Room { room_id: bound } = &manifest.scope
        && room_id != Some(bound.as_str())
    {
        return Err(format!("\"{}\" belongs to a specific room or space. Open it in its original context.", manifest.name));
    }
    if let Some(room_id) = room_id {
        if !manifest.can_run_in_context(room_id, is_space) {
            let context = if manifest.runs_in() == RunsIn::Spaces { "a space" } else { "a room" };
            return Err(format!("Run \"{}\" in {context}.", manifest.name));
        }
    } else if manifest.runs_in() == RunsIn::Room {
        return Err(format!("Choose a room to run \"{}\".", manifest.name));
    }
    Ok(())
}

/// Resolves the actual Matrix context type instead of treating every room ID
/// as a timeline room; spaces use the same ID type and need a modal surface.
fn app_launch_context(manifest: &MiniAppManifest, room_id: Option<&RoomId>) -> Result<bool, String> {
    let is_space = match room_id {
        Some(room_id) => {
            let room = crate::sliding_sync::get_client().and_then(|client| client.get_room(room_id))
                .filter(|room| room.state() == RoomState::Joined)
                .ok_or("The mini-app's room or space is no longer joined.")?;
            room.is_space()
        }
        None => false,
    };
    validate_app_launch_context(manifest, room_id.map(RoomId::as_str), is_space)?;
    Ok(is_space)
}

/// Invalidate queued watch events when the OS suspends the app.
pub(super) fn stop_background_watches() {
    super::room_watch::stop_all();
    with_a2app(|state| state.watched_rooms.clear());
}

pub(super) fn refresh_background_watches() { prune_hook_subs(); }

/// Forgets subscriptions whose isolate is gone or whose grant was pulled,
/// and starts or stops the worker's watches to match what's left.
fn prune_hook_subs() {
    let background_rooms = super::background::watched_rooms();
    with_a2app(|state| {
        let A2AppState { hook_subs, registry, permissions, watched_rooms, account_watched, .. } = state;
        hook_subs.retain(|heap, sub| {
            if instances::key_of_heap(*heap).is_none() {
                return false;
            }
            let Some(manifest) = registry.get(&sub.app_id) else { return false };
            sub.hooks.retain(|hook| {
                a2app_core::capabilities::for_hook(hook)
                    .is_some_and(|cap| {
                        let context = PermissionContext {
                            origin_room: sub.room_id.as_deref().map(|r| r.as_str()),
                            target_room: sub.room_id.as_deref().map(|r| r.as_str()),
                        };
                        let effective = if services::is_room_collection(cap) {
                            permissions.effective_collection_capability_in_context(manifest, cap, context)
                        } else { permissions.effective_capability_in_context(manifest, cap, context) };
                        effective == Effective::Granted
                    })
            });
            !sub.hooks.is_empty()
        });
        let wanted: HashSet<OwnedRoomId> = hook_subs.values().filter_map(|s| s.room_id.clone()).chain(background_rooms).collect();
        for room_id in watched_rooms.difference(&wanted) {
            submit_async_request(MatrixRequest::A2App(A2AppMatrixRequest::UnwatchRoom { room_id: room_id.clone() }));
        }
        for room_id in wanted.difference(watched_rooms) {
            submit_async_request(MatrixRequest::A2App(A2AppMatrixRequest::WatchRoom { room_id: room_id.clone() }));
        }
        *watched_rooms = wanted;
        let wants_account = hook_subs.values().any(|s| s.hooks.iter().any(|hook| ACCOUNT_HOOKS.contains(hook)));
        if wants_account != *account_watched {
            submit_async_request(MatrixRequest::A2App(A2AppMatrixRequest::WatchAccount { watch: wants_account }));
            *account_watched = wants_account;
        }
    });
}

/// Hands each watch's news to the isolates subscribed to it.
fn deliver_room_hooks(
    cx: &mut Cx,
    ui: &WidgetRef,
    events: Vec<A2AppRoomWatchEvent>,
    account_events: Vec<A2AppAccountWatchEvent>,
    host_events: Vec<(&'static str, serde_json::Value)>,
) {
    prune_hook_subs();
    let show_receipts = cx.global::<AppPreferencesGlobal>().0.show_read_receipts;
    let show_typing = cx.global::<AppPreferencesGlobal>().0.show_typing_notices;
    // Messages and receipts batch per pass, reactions, edits and invites get
    // a call each, the rest coalesce to the latest; a `None` room is account-wide.
    let mut messages: HashMap<OwnedRoomId, Vec<serde_json::Value>> = HashMap::new();
    let mut receipts: HashMap<OwnedRoomId, Vec<serde_json::Value>> = HashMap::new();
    let mut each: Vec<(Option<OwnedRoomId>, &'static str, serde_json::Value)> = Vec::new();
    let mut latest: HashMap<(Option<OwnedRoomId>, &'static str), serde_json::Value> = HashMap::new();
    let mut closed: Vec<OwnedRoomId> = Vec::new();
    for event in events {
        if !event.is_current() { continue; }
        let room_id = event.room_id;
        match event.kind {
            RoomWatchKind::Message { event_id, sender, sender_name, body, msgtype, ts, is_own } => {
                messages.entry(room_id.clone()).or_default().push(serde_json::json!({
                    "room_id": room_id,
                    "event_id": event_id,
                    "sender": sender.localpart(),
                    "sender_id": sender,
                    "sender_name": sender_name,
                    "body": body,
                    "ts": ts,
                    "msgtype": msgtype,
                    "is_own": is_own,
                }));
            }
            RoomWatchKind::MessageChanged { event_id, edited, redacted, body } => {
                each.push((Some(room_id.clone()), "on_room_message_changed", serde_json::json!({
                    "room_id": room_id,
                    "event_id": event_id,
                    "edited": edited,
                    "redacted": redacted,
                    "body": body,
                })));
            }
            RoomWatchKind::Reaction { event_id, key, sender, added } => {
                let is_own = crate::sliding_sync::current_user_id().as_ref() == Some(&sender);
                each.push((Some(room_id.clone()), "on_room_reaction", serde_json::json!({
                    "room_id": room_id,
                    "event_id": event_id,
                    "key": key,
                    "sender_id": sender,
                    "is_own": is_own,
                    "added": added,
                })));
            }
            // The user's "show when others are typing" switch hides typing from apps too.
            RoomWatchKind::Typing { .. } if !show_typing => {}
            RoomWatchKind::Typing { users } => {
                let typing: Vec<_> = users.iter()
                    .map(|(user_id, name)| serde_json::json!({ "user_id": user_id, "name": name }))
                    .collect();
                latest.insert(
                    (Some(room_id.clone()), "on_room_typing"),
                    serde_json::json!({ "room_id": room_id, "typing": typing }),
                );
            }
            RoomWatchKind::Receipts { receipts: batch } => {
                receipts.entry(room_id).or_default().extend(batch.into_iter().map(|r| {
                    serde_json::json!({ "user_id": r.user_id, "event_id": r.event_id, "ts": r.ts })
                }));
            }
            RoomWatchKind::MembersChanged { count } => {
                latest.insert(
                    (Some(room_id.clone()), "on_room_members_changed"),
                    serde_json::json!({ "room_id": room_id, "count": count }),
                );
            }
            RoomWatchKind::PinsChanged { pinned } => {
                latest.insert(
                    (Some(room_id.clone()), "on_room_pins_changed"),
                    serde_json::json!({ "room_id": room_id, "pinned": pinned }),
                );
            }
            RoomWatchKind::InfoChanged { name, topic, encrypted, is_favorite, is_low_priority, upgraded } => {
                latest.insert((Some(room_id.clone()), "on_room_info_changed"), serde_json::json!({
                    "room_id": room_id,
                    "name": name,
                    "topic": topic,
                    "encrypted": encrypted,
                    "is_favorite": is_favorite,
                    "is_low_priority": is_low_priority,
                    "upgraded": upgraded,
                }));
            }
            RoomWatchKind::UnreadChanged { unread, mentions, marked_unread } => {
                latest.insert((Some(room_id.clone()), "on_room_unread_changed"), serde_json::json!({
                    "room_id": room_id,
                    "unread": unread,
                    "mentions": mentions,
                    "marked_unread": marked_unread,
                }));
            }
            RoomWatchKind::Closed => closed.push(room_id),
        }
    }
    let mut rooms_changed: Option<(Vec<OwnedRoomId>, Vec<OwnedRoomId>, Vec<OwnedRoomId>)> = None;
    for event in account_events {
        match event.kind {
            AccountWatchKind::RoomsChanged { joined, left, changed } => {
                let (all_joined, all_left, all_changed) = rooms_changed.get_or_insert_default();
                all_joined.extend(joined);
                all_left.extend(left);
                all_changed.extend(changed);
            }
            AccountWatchKind::InviteReceived { room_id, name, inviter, inviter_name, is_space } => {
                each.push((None, "on_invite_received", serde_json::json!({
                    "room_id": room_id,
                    "name": name,
                    "inviter_id": inviter,
                    "inviter_name": inviter_name,
                    "is_space": is_space,
                })));
            }
            AccountWatchKind::UnreadTotalsChanged { unread, mentions } => {
                latest.insert(
                    (None, "on_unread_totals_changed"),
                    serde_json::json!({ "unread": unread, "mentions": mentions }),
                );
            }
        }
    }
    if let Some((joined, left, changed)) = rooms_changed {
        latest.insert(
            (None, "on_rooms_changed"),
            serde_json::json!({ "joined": joined, "left": left, "changed": changed }),
        );
    }
    for (hook, payload) in host_events {
        latest.insert((None, hook), payload);
    }
    let subs: Vec<(usize, String, Option<OwnedRoomId>, HashSet<&'static str>)> = with_a2app(|state| {
        state.hook_subs.iter().map(|(heap, s)| (*heap, s.app_id.clone(), s.room_id.clone(), s.hooks.clone())).collect()
    }).unwrap_or_default();
    for (room, batch) in &messages { super::background::room_messages(cx, room, batch); }
    let mut delivered = false;
    for (heap, app_id, room_id, hooks) in subs {
        if let Some(room_id) = &room_id {
            if !room_policy_allows(room_id.as_str(), RoomAccess::Read) { continue; }
            if hooks.contains("on_room_message")
                && !super::background::handles_messages(heap)
                && let Some(batch) = messages.get(room_id)
            {
                let payload = serde_json::Value::Array(batch.clone()).to_string();
                delivered |= instances::call_hook_by_heap(cx, heap, live_id!(on_room_message), &[&payload]);
            }
            if show_receipts
                && hooks.contains("on_room_receipt")
                && let Some(batch) = receipts.get(room_id)
            {
                let payload = serde_json::json!({ "room_id": room_id, "receipts": batch }).to_string();
                delivered |= instances::call_hook_by_heap(cx, heap, live_id!(on_room_receipt), &[&payload]);
            }
        }
        // A room's payloads go to its own subscribers; account-wide ones to anyone with the hook.
        let mine = |room: &Option<OwnedRoomId>, hook: &&'static str| {
            (room.is_none() || *room == room_id) && hooks.contains(hook)
        };
        for (_, hook, payload) in each.iter().filter(|(room, hook, _)| mine(room, hook)) {
            if let Some(payload) = permission_filtered_hook(&app_id, room_id.as_deref(), hook, payload) {
                delivered |= instances::call_hook_by_heap(cx, heap, LiveId::from_str(hook), &[&payload]);
            }
        }
        for ((_, hook), payload) in latest.iter().filter(|((room, hook), _)| mine(room, hook)) {
            if let Some(payload) = permission_filtered_hook(&app_id, room_id.as_deref(), hook, payload) {
                delivered |= instances::call_hook_by_heap(cx, heap, LiveId::from_str(hook), &[&payload]);
            }
        }
    }
    if !closed.is_empty() {
        with_a2app(|state| {
            state.hook_subs.retain(|_, s| !s.room_id.as_ref().is_some_and(|r| closed.contains(r)));
            for room_id in &closed {
                state.watched_rooms.remove(room_id);
            }
        });
    }
    if delivered {
        // Same as a service reply: the hook likely re-rendered the app.
        SignalToUI::set_ui_signal();
        let _ = cx.new_next_frame();
        ui.redraw(cx);
    }
}

/// Live events use the same target-room boundary as requested data. Even
/// navigation and account hooks must not disclose a protected room's name.
fn permission_filtered_hook(app_id: &str, origin: Option<&RoomId>, hook: &str, payload: &serde_json::Value) -> Option<String> {
    with_a2app(|state| {
        let manifest = state.registry.get(app_id)?;
        let cap = a2app_core::capabilities::for_hook(hook)?;
        let allowed = |room: &str| {
            state.permissions.room_policy(Some(room), RoomAccess::Read) != PolicyDecision::Deny
                && state.permissions.effective_capability_in_context(manifest, cap, PermissionContext {
                    origin_room: origin.map(|r| r.as_str()), target_room: Some(room),
                }) == Effective::Granted
        };
        let mut payload = payload.clone();
        if payload.get("room_id").or_else(|| payload.get("space_id")).and_then(serde_json::Value::as_str)
            .is_some_and(|room| !allowed(room)) { return None; }
        for key in ["joined", "left", "changed"] {
            if let Some(rooms) = payload.get_mut(key).and_then(serde_json::Value::as_array_mut) {
                rooms.retain(|room| room.as_str().is_some_and(&allowed));
            }
        }
        // Totals computed for the entire account cannot be safely attributed
        // to a subject with access to only selected rooms.
        if hook == "on_unread_totals_changed"
            && ((state.permissions.effective_capability(manifest, cap) != Effective::Granted
                && !(state.permissions.has_all_room_collection_consent(app_id, cap, PermissionContext {
                    origin_room: origin.map(|room| room.as_str()), target_room: None,
                }) && state.permissions.effective_collection_capability_in_context(manifest, cap, PermissionContext {
                    origin_room: origin.map(|room| room.as_str()), target_room: None,
                }) == Effective::Granted))
                || state.permissions.policy_mode(RoomAccess::Read) == RoomPolicyMode::WhitelistOnly
                || state.permissions.room_rules().values().any(|rule| rule.read == PolicyDecision::Deny)
                || state.permissions.space_rules().values().any(|rule| rule.read == PolicyDecision::Deny))
        { return None; }
        Some(payload.to_string())
    }).flatten()
}

/// Drops every instance of the app, on every surface.
///
/// Terminating an instance also removes any room pane showing it.
fn stop_app_everywhere(cx: &mut Cx, ui: &WidgetRef, app_id: &str) {
    host_pane(cx, ui).drop_app(cx, app_id);
    cancel_subject_permission_prompts(cx, ui, app_id);
    instances::quit_app(cx, app_id);
}

fn apply_op(cx: &mut Cx, ui: &WidgetRef, op: A2AppOp) {
    match op {
        A2AppOp::ReviewFlow(context) => {
            // Park the modal instance so its pending action remains reviewable.
            host_pane(cx, ui).close_active(cx, true);
            with_a2app(|state| state.foreground_app = None);
            ui.modal(cx, ids!(mini_app_host_modal)).close(cx);
            cx.action(NavigationBarAction::GoToMiniApps);
            cx.action(A2AppRuntimeAction::ReviewFlow(context));
            ui.redraw(cx);
        }
        A2AppOp::OpenAppFromContext { context, flow_epoch, app_id, room_id } => {
            let result = super::information_flow::current_context(&context).and_then(|()| {
                a2app_core::information_flow::commit_exact_action_for_activation(&context, flow_epoch, &a2app_core::information_flow::SensitiveAction {
                    kind: "apps.launch".into(), target: app_id.clone(),
                }, &serde_json::json!({ "app_id": app_id, "room_id": room_id }))
            });
            if let Err(error) = result {
                enqueue_popup_notification(error, PopupKind::Error, Some(6.0));
                return;
            }
            apply_op(cx, ui, A2AppOp::OpenApp { app_id, room_id: Some(room_id), in_room_pane: true });
        }
        A2AppOp::OpenApp { app_id, room_id, in_room_pane } => {
            let Some((manifest, grants, restricted, room)) = with_a2app(|state| {
                let manifest = state.registry.get(&app_id).cloned();
                let grants = a2app_core::permissions::snapshot_grants_for(&app_id);
                let restricted = state.permissions.is_restricted(&app_id);
                let room = room_id.or_else(|| match manifest.as_ref().map(|m| &m.scope) {
                    Some(A2AppScope::Room { room_id }) => OwnedRoomId::try_from(room_id.as_str()).ok(),
                    _ => None,
                });
                manifest.map(|m| (m, grants, restricted, room))
            }).flatten() else {
                enqueue_popup_notification("That mini-app no longer exists.", PopupKind::Error, Some(4.0));
                return;
            };
            let is_space = match app_launch_context(&manifest, room.as_deref()) {
                Ok(is_space) => is_space,
                Err(error) => {
                    enqueue_popup_notification(error, PopupKind::Error, Some(5.0));
                    return;
                }
            };
            if restricted {
                enqueue_popup_notification(
                    format!("\"{}\" was stopped for hammering the host with requests. You can let it run again from its app info.", manifest.name),
                    PopupKind::Warning, Some(6.0),
                );
                return;
            }
            let key = (app_id.clone(), room.clone());
            if instances::context_of_key(&key).is_none() { permission_batch::clear_dismissals(&app_id); }
            if matches!(instances::context_of_key(&key), Some(a2app_core::information_flow::ContextId::PublicApp { .. })) {
                host_pane(cx, ui).drop_app(cx, &app_id);
                instances::quit(cx, &key);
            }
            match (in_room_pane && !is_space, room) {
                // One isolate per (app, room): the target room's dock opens
                // (or restores) ITS OWN instance, independent of any other.
                (true, Some(pane_room)) => open_in_room_pane(cx, app_id, pane_room),
                (_, room) => {
                    if host_pane(cx, ui).open_app(cx, &manifest, grants, room.clone()) {
                        with_a2app(|state| state.foreground_app = Some(app_id.clone()));
                        ui.modal(cx, ids!(mini_app_host_modal)).open(cx);
                    } else if let Some(room_id) = room {
                        // Shown in a room already: bring that up instead.
                        open_in_room_pane(cx, app_id, room_id);
                    }
                }
            }
        }
        A2AppOp::OpenPublicApp(app_id) => {
            let Some((manifest, restricted)) = with_a2app(|state| state.registry.get(&app_id)
                .map(|manifest| (manifest.clone(), state.permissions.is_restricted(&app_id)))).flatten()
            else { return };
            if let Err(error) = validate_app_launch_context(&manifest, None, false) {
                enqueue_popup_notification(format!("Couldn't open a public instance: {error}"), PopupKind::Error, Some(5.0));
                return;
            }
            if restricted {
                enqueue_popup_notification("Allow this stopped app to run before opening a public instance.", PopupKind::Error, Some(5.0));
                return;
            }
            let key = (app_id.clone(), None);
            permission_batch::clear_dismissals(&app_id);
            host_pane(cx, ui).drop_app(cx, &app_id);
            if instances::terminate(cx, &key) { app_stopped(cx, &app_id); }
            let grants = grants_in_room(&app_id, None);
            if instances::ensure_public(cx, &manifest, &grants).is_none() { return; }
            let mut display = manifest;
            display.name = format!("{} · Public instance", display.name);
            if host_pane(cx, ui).open_app(cx, &display, grants, None) {
                with_a2app(|state| state.foreground_app = Some(app_id));
                ui.modal(cx, ids!(mini_app_host_modal)).open(cx);
            }
        }
        A2AppOp::CloseHostPane => {
            // Closing quits the app.
            with_a2app(|state| state.foreground_app = None);
            if let Some((app_id, _)) = host_pane(cx, ui).close_active(cx, false) {
                app_stopped(cx, &app_id);
            }
            ui.modal(cx, ids!(mini_app_host_modal)).close(cx);
            ui.redraw(cx);
        }
        A2AppOp::SaveBackgroundTask { binding, trigger, expected_fingerprint } => {
            if let Err(error) = super::background::save(cx, binding, trigger, expected_fingerprint) {
                enqueue_popup_notification(error, PopupKind::Error, Some(8.0));
            }
            ui.redraw(cx);
        }
        A2AppOp::SetBackgroundTaskEnabled { id, enabled, expected_fingerprint } => {
            if let Err(error) = super::background::set_enabled(cx, id, enabled, expected_fingerprint) {
                enqueue_popup_notification(error, PopupKind::Error, Some(8.0));
            }
            ui.redraw(cx);
        }
        A2AppOp::RunBackgroundTask(id) => {
            if let Err(error) = super::background::run_now(cx, id) { enqueue_popup_notification(error, PopupKind::Error, Some(8.0)); }
            ui.redraw(cx);
        }
        A2AppOp::RemoveBackgroundTask(id) => {
            if let Err(error) = super::background::remove(cx, id) { enqueue_popup_notification(error, PopupKind::Error, Some(8.0)); }
            ui.redraw(cx);
        }
        A2AppOp::ForceStop(app_id) => {
            super::background::disable_app(cx, &app_id);
            stop_app_everywhere(cx, ui, &app_id);
            let was_foreground = with_a2app(|state| {
                // One-time grants die with the isolate.
                state.permissions.clear_once_for(&app_id);
                state.mark_perms_dirty();
                state.foreground_app.take_if(|f| *f == app_id).is_some()
            }).unwrap_or(false);
            if was_foreground {
                ui.modal(cx, ids!(mini_app_host_modal)).close(cx);
            }
            publish_grants(cx);
            ui.redraw(cx);
        }
        A2AppOp::Uninstall(app_id) => {
            super::background::disable_app(cx, &app_id);
            let Some(Some(manifest)) = with_a2app(|state| state.registry.get(&app_id).cloned()) else { return };
            if manifest.builtin {
                enqueue_popup_notification("Built-in mini-apps can't be uninstalled.", PopupKind::Warning, Some(4.0));
                return;
            }
            let archived_bundle = match bundle::try_to_text(&manifest) {
                Ok(bundle) => bundle,
                Err(error) => {
                    enqueue_popup_notification(format!("Could not archive this mini-app's complete history: {error}"), PopupKind::Error, Some(6.0));
                    return;
                }
            };
            let archived = with_a2app(|state| {
                let mut archived = state.persisted.clone();
                archived.archived.retain(|app| app.id != app_id);
                archived.archived.push(manifest.clone());
                archived.archived_bundles.insert(app_id.clone(), archived_bundle);
                archived
            });
            let Some(archived) = archived else { return };
            if let Err(error) = persistence::save_registry_state(&archived) {
                enqueue_popup_notification(format!("Could not save the archived history: {error}"), PopupKind::Error, Some(6.0));
                return;
            }
            stop_app_everywhere(cx, ui, &app_id);
            with_a2app(|state| {
                state.persisted = archived;
                state.imported_room_apps.retain(|_, installed| installed != &app_id);
                state.registry.remove(&app_id);
                state.permissions.remove_app(&app_id);
                state.foreground_app.take_if(|f| *f == app_id);
                state.perms_dirty = true;
                state.registry_dirty = true;
            });
            persistence::remove_user_app(&app_id);
            if let Err(error) = persistence::clear_app_data(&app_id) {
                enqueue_popup_notification(error, PopupKind::Error, Some(6.0));
            }
            publish_grants(cx);
            cx.action(A2AppRuntimeAction::Uninstalled(app_id.clone()));
            enqueue_popup_notification(
                format!("Uninstalled \"{}\". Its bundle was archived.", manifest.name),
                PopupKind::Success, Some(4.0),
            );
            ui.redraw(cx);
        }
        A2AppOp::ClearData(app_id) => {
            stop_app_everywhere(cx, ui, &app_id);
            if let Err(error) = persistence::clear_app_data(&app_id) {
                enqueue_popup_notification(error, PopupKind::Error, Some(6.0));
                return;
            }
            enqueue_popup_notification("Cleared this mini-app's saved data.", PopupKind::Success, Some(3.0));
            ui.redraw(cx);
        }
        A2AppOp::Export(app_id) => {
            let Some(Some(manifest)) = with_a2app(|state| state.registry.get(&app_id).cloned()) else { return };
            let text = match bundle::try_to_text(&manifest) {
                Ok(text) => text,
                Err(error) => { enqueue_popup_notification(format!("Export failed: {error}"), PopupKind::Error, Some(5.0)); return; }
            };
            cx.copy_to_clipboard(&text);
            match bundle::write_export(&manifest) {
                Ok(path) => enqueue_popup_notification(
                    format!("Exported to {} and copied to the clipboard.", path.display()),
                    PopupKind::Success, Some(5.0),
                ),
                Err(e) => enqueue_popup_notification(
                    format!("Copied to the clipboard, but the file export failed: {e}"),
                    PopupKind::Warning, Some(5.0),
                ),
            }
        }
        A2AppOp::ImportText(text) => {
            let actor = current_version_actor(cx);
            install_import(cx, ui, bundle::parse_with_history(&text), AcquisitionSource::Clipboard, actor);
        }
        A2AppOp::ImportRoomBundle { text, room_id, event_id, sender_id, sender_name, shared_at_unix } => {
            let actor = current_version_actor(cx);
            let source = match room_id {
                Some(room_id) => AcquisitionSource::RoomAttachment {
                    room_id: room_id.to_string(), event_id: event_id.map(|id| id.to_string()),
                    media_uri: None, file_name: "Shared mini-app".into(), shared_at_unix,
                    sender: Some(VersionActor { user_id: sender_id, display_name: Some(sender_name) }),
                },
                None => AcquisitionSource::Clipboard,
            };
            install_import(cx, ui, bundle::parse_with_history(&text), source, actor);
        }
        A2AppOp::ImportRoomAttachment { media_source, filename, size, room_id, event_id, sender_id, sender_name, shared_at_unix } => {
            start_room_import(cx, ui, media_source, filename, size, room_id, event_id, sender_id, sender_name, shared_at_unix);
        }
        A2AppOp::RoomAttachmentDownloaded { request_id, media_uri, account, actor, source, text } => {
            let pending = with_a2app(|state| {
                if state.room_imports.get(&media_uri) != Some(&request_id) { return false; }
                state.room_imports.remove(&media_uri);
                true
            }).unwrap_or(false);
            if !pending { return; }
            if crate::sliding_sync::current_user_id().is_none_or(|id| id.as_str() != account) {
                enqueue_popup_notification("The account changed before the mini-app finished downloading. Add it again from this account.", PopupKind::Info, Some(5.0));
                ui.redraw(cx);
                return;
            }
            install_import(cx, ui, text.and_then(|text| bundle::parse_with_history(&text)), source, actor);
            ui.redraw(cx);
        }
        A2AppOp::ImportFile(path) => {
            let parsed = std::fs::metadata(&path)
                .map_err(|e| format!("Couldn't read that file: {e}"))
                .and_then(|metadata| if metadata.len() > bundle::MAX_BUNDLE_BYTES as u64 {
                    Err("That mini-app file is too large.".into())
                } else { std::fs::read_to_string(&path).map_err(|e| format!("Couldn't read that file: {e}")) })
                .and_then(|text| bundle::parse_with_history(&text));
            let file_name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let actor = current_version_actor(cx);
            let path = path.canonicalize().unwrap_or(path).to_string_lossy().into_owned();
            install_import(cx, ui, parsed, AcquisitionSource::File { file_name, path: Some(path) }, actor);
        }
        A2AppOp::SwitchVersion { app_id, stamp } => {
            let actor = current_version_actor(cx);
            let switched = with_a2app(|state| {
                let mut manifest = state.registry.get(&app_id).cloned()?;
                let (version, source) = persistence::load_version(&app_id, &stamp)?;
                // A current bundled default does not make old source public.
                // Restore its retained floor before making it executable.
                if let Err(error) = manifest_flow_context(&manifest)
                    .and_then(|_| a2app_core::information_flow::restore_app_code_provenance(&app_id))
                {
                    enqueue_popup_notification(error, PopupKind::Error, Some(5.0));
                    return None;
                }
                if let Err(error) = archive_current(&mut manifest) {
                    enqueue_popup_notification(error, PopupKind::Error, Some(5.0));
                    return None;
                }
                let label = versions::label_for(version.at_unix, utc_offset_secs());
                let mut updated = version.apply_to(&manifest, source);
                let note = format!("Restored version {} from {label}", version.stamp);
                if let Err(error) = commit_version(&mut updated, VersionOrigin::Restore, &note, actor, None) {
                    enqueue_popup_notification(error, PopupKind::Error, Some(5.0));
                    return None;
                }
                Some((updated, label))
            }).flatten();
            match switched {
                Some((updated, label)) => {
                    let done = format!("\"{}\" now runs its version from {label}.", updated.name);
                    install_version(cx, ui, updated, done);
                }
                None => enqueue_popup_notification("Couldn't load that version.", PopupKind::Error, Some(4.0)),
            }
        }
        A2AppOp::SaveSource { app_id, source } => {
            let Some(mut base) = with_a2app(|state| state.registry.get(&app_id).cloned()).flatten() else {
                enqueue_popup_notification("That mini-app no longer exists.", PopupKind::Error, Some(4.0));
                return;
            };
            if base.source == source {
                enqueue_popup_notification("Nothing changed.", PopupKind::Info, Some(3.0));
                return;
            }
            if let Err(error) = super::information_flow::record_source_edit(&base) {
                enqueue_popup_notification(error, PopupKind::Error, Some(6.0));
                return;
            }
            if let Err(error) = archive_current(&mut base) {
                enqueue_popup_notification(error, PopupKind::Error, Some(5.0));
                return;
            }
            let mut updated = a2app_core::manifest::rewritten(&base, source);
            if let Err(error) = commit_version(&mut updated, VersionOrigin::Manual, "Edited by hand", current_version_actor(cx), None) {
                enqueue_popup_notification(error, PopupKind::Error, Some(5.0));
                return;
            }
            install_version(cx, ui, updated, String::from("Saved your edit as a new version."));
        }
        A2AppOp::ResetToStock(app_id) => {
            let actor = current_version_actor(cx);
            let reset = with_a2app(|state| {
                let current = state.registry.get(&app_id).cloned()?;
                let stock = builtin::stock(&app_id)?;
                if builtin::matches_default(&current, &stock) {
                    return Some(None);
                }
                Some(Some(on_stock(current, stock, actor)))
            }).flatten();
            match reset {
                Some(Some(Ok(updated))) => install_version(cx, ui, updated, String::from("Back on the stock version.")),
                Some(Some(Err(error))) => enqueue_popup_notification(error, PopupKind::Error, Some(5.0)),
                Some(None) => enqueue_popup_notification("This app is already on its stock version.", PopupKind::Info, Some(3.0)),
                None => enqueue_popup_notification("Only built-in apps have a stock version.", PopupKind::Error, Some(4.0)),
            }
        }
        A2AppOp::ShareBundle(app_id) => {
            let Some(Some(manifest)) = with_a2app(|state| state.registry.get(&app_id).cloned()) else { return };
            let shared = bundle::write_export(&manifest).and_then(|path| {
                robius_share::ShareSheet::new()
                    .set_title(format!("{} mini-app", manifest.name))
                    .set_subject(bundle::share_caption(&manifest))
                    .add_file_with_mime_type(&path, bundle::BUNDLE_MIME)
                    .share()
                    .map_err(|e| format!("couldn't open the share sheet: {e}"))
            });
            if let Err(e) = shared {
                enqueue_popup_notification(format!("Sharing failed: {e}"), PopupKind::Error, Some(5.0));
            }
        }
        A2AppOp::SendToRoom { app_id, room_id } => {
            let Some(Some(manifest)) = with_a2app(|state| state.registry.get(&app_id).cloned()) else { return };
            let staged = bundle::write_export(&manifest).and_then(|path| {
                let caption = bundle::share_caption(&manifest);
                queue_room_action(cx, room_id, RoomAction::StageAttachment { path, caption })
            });
            if let Err(e) = staged {
                enqueue_popup_notification(format!("Couldn't send the bundle: {e}"), PopupKind::Error, Some(5.0));
            }
        }
        A2AppOp::OpenInRoom { app_id, room_id } => {
            let Some(manifest) = with_a2app(|state| state.registry.get(&app_id).cloned()).flatten() else {
                enqueue_popup_notification("That mini-app no longer exists.", PopupKind::Error, Some(4.0));
                return;
            };
            match app_launch_context(&manifest, Some(&room_id)) {
                Ok(true) => {
                    apply_op(cx, ui, A2AppOp::OpenApp { app_id, room_id: Some(room_id), in_room_pane: false });
                    return;
                }
                Ok(false) => {}
                Err(error) => {
                    enqueue_popup_notification(error, PopupKind::Error, Some(5.0));
                    return;
                }
            }
            if let Err(e) = queue_room_action(cx, room_id, RoomAction::OpenApp(app_id)) {
                enqueue_popup_notification(format!("Couldn't open it there: {e}"), PopupKind::Error, Some(5.0));
            }
        }
        A2AppOp::SetPermission { app_id, perm, state: new_state } => {
            if new_state == GrantState::Ask {
                permission_lifecycle::ask_again(cx, ui, &app_id, perm);
                return;
            }
            with_a2app(|state| {
                state.permissions.set(&app_id, perm, new_state);
                state.perms_dirty = true;
            });
            publish_grants(cx);
            apply_permission_to_running(cx, ui, &app_id, perm);
            ui.redraw(cx);
        }
        A2AppOp::ResetAllPermissions => reset_all_permissions(cx, ui),
        A2AppOp::ResetAppPermissions(app_id) => permission_lifecycle::reset_app_permissions(cx, ui, &app_id),
        A2AppOp::SetCapability { app_id, cap_id, state: new_state } => {
            if new_state == GrantState::Ask {
                permission_lifecycle::ask_again_capability(cx, ui, &app_id, &cap_id);
                return;
            }
            let Some(cap) = a2app_core::capabilities::by_id(&cap_id) else { return };
            with_a2app(|state| {
                state.permissions.set_capability(&app_id, cap.id, new_state);
                state.perms_dirty = true;
            });
            publish_grants(cx);
            if let Some(group) = cap.group {
                apply_permission_to_running(cx, ui, &app_id, group);
            }
            ui.redraw(cx);
        }
        A2AppOp::GrantScoped { app_id, perm, cap_id, scope, duration, origin_room } => {
            let result = with_a2app(|state| {
                if cap_id.is_some() && state.permissions.state(&app_id, perm) == GrantState::Denied {
                    return Err("This permission group is blocked. Change the group to Ask before allowing one ability.".to_string());
                }
                let result = state.permissions.grant_scoped(&app_id, perm, cap_id.as_deref(), scope, duration, origin_room.as_deref());
                if result.is_ok() {
                    // A scoped choice replaces a previous blanket Allow.
                    state.permissions.set(&app_id, perm, GrantState::Ask);
                    if let Some(cap) = cap_id.as_deref() {
                        state.permissions.set_capability(&app_id, cap, GrantState::Ask);
                    } else {
                        for cap in a2app_core::capabilities::CATALOG.iter().filter(|cap| cap.group == Some(perm)) {
                            if state.permissions.capability_state(&app_id, cap.id) == GrantState::Granted {
                                state.permissions.set_capability(&app_id, cap.id, GrantState::Ask);
                            }
                        }
                    }
                    state.perms_dirty = true;
                }
                result
            });
            if let Some(Err(error)) = result {
                enqueue_popup_notification(error, PopupKind::Error, Some(5.0));
            }
            publish_grants(cx);
            apply_permission_to_running(cx, ui, &app_id, perm);
            ui.redraw(cx);
        }
        A2AppOp::GrantNetwork { app_id, network, scope, duration, origin_room } => {
            let result = with_a2app(|state| {
                let result = state.permissions.allow_network(&app_id, network, scope, duration, origin_room.as_deref());
                if result.is_ok() {
                    state.permissions.set(&app_id, Permission::Network, GrantState::Ask);
                    state.permissions.set_capability(&app_id, "network.http", GrantState::Ask);
                }
                state.perms_dirty = true;
                result
            });
            if let Some(Err(error)) = result {
                enqueue_popup_notification(error, PopupKind::Error, Some(5.0));
            }
            publish_grants(cx);
            ui.redraw(cx);
        }
        A2AppOp::RevokeScopedGrant(id) => {
            let subject = with_a2app(|state| {
                let subject = state.permissions.scoped_grant(id)?.subject.clone();
                state.permissions.revoke_scoped_grant(id);
                state.perms_dirty = true;
                Some(subject)
            }).flatten();
            if let Some(subject) = subject { permission_lifecycle::retire_after_revocation(cx, ui, &subject); }
        }
        A2AppOp::RevokeNetworkGrant(id) => {
            let subject = with_a2app(|state| {
                let subject = state.permissions.network_grant(id)?.subject.clone();
                state.permissions.revoke_network_grant(id);
                state.perms_dirty = true;
                Some(subject)
            }).flatten();
            if let Some(subject) = subject { permission_lifecycle::retire_after_revocation(cx, ui, &subject); }
        }
        A2AppOp::ClearLegacyGrants { subject, perm } => {
            with_a2app(|state| {
                let store = &mut state.permissions;
                match perm {
                    Permission::Network => { for host in store.host_grants(&subject) { store.disallow_host(&subject, &host); } }
                    Permission::MatrixRoomsRead => { for room in store.room_read_grants(&subject) { store.disallow_room_read(&subject, &room); } }
                    Permission::MatrixRoomsSend => { for room in store.room_send_grants(&subject) { store.disallow_room_send(&subject, &room); } }
                    Permission::McpTools => { store.clear_persistent_tool_grants_for(&subject); }
                    _ => {}
                }
                state.perms_dirty = true;
            });
            permission_lifecycle::retire_after_revocation(cx, ui, &subject);
        }
        A2AppOp::SetGlobalPolicy { access, decision } => {
            with_a2app(|state| { state.permissions.set_global_policy(access, decision); state.perms_dirty = true; });
            refresh_permission_policy(cx, ui);
        }
        A2AppOp::SetPolicyMode { access, mode } => {
            with_a2app(|state| { state.permissions.set_policy_mode(access, mode); state.perms_dirty = true; });
            refresh_permission_policy(cx, ui);
        }
        A2AppOp::SetMatrixWrite(enabled) => {
            with_a2app(|state| { state.permissions.set_matrix_write(enabled); state.perms_dirty = true; });
            refresh_permission_policy(cx, ui);
        }
        A2AppOp::SetRoomPolicy { room_id, access, decision } => {
            with_a2app(|state| { state.permissions.set_room_policy(&room_id, access, decision); state.perms_dirty = true; });
            refresh_permission_policy(cx, ui);
        }
        A2AppOp::SetSpacePolicy { space_id, access, decision } => {
            with_a2app(|state| { state.permissions.set_space_policy(&space_id, access, decision); state.perms_dirty = true; });
            refresh_permission_policy(cx, ui);
        }
        A2AppOp::SetFlowPolicy { source, policy } => {
            if let Err(error) = ensure_source_owner(&source).and_then(|()| a2app_core::information_flow::set_policy(source, policy)) {
                enqueue_popup_notification(error, PopupKind::Error, Some(6.0));
                return;
            }
            with_a2app(|state| { state.generation = None; state.pending_generated = None; });
            #[cfg(unix)]
            {
                let rooms = with_a2app(|state| state.ai_sessions.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
                for room in rooms {
                    abort_ai_room_work(cx, ui, &room);
                }
            }
            refresh_permission_policy(cx, ui);
        }
        A2AppOp::GrantFlowSharing { source, recipient, reader, duration } => {
            let checked = (|| {
                use a2app_core::information_flow::{ReaderScope, SharingDuration, Recipient};
                ensure_source_owner(&source)?;
                let account = super::information_flow::account()?;
                let reader_account = match &reader {
                    ReaderScope::AllReaders => None,
                    ReaderScope::App { account, .. } => Some(account.as_str()),
                    ReaderScope::Context(context) => Some(context.account()),
                };
                if reader_account.is_some_and(|owner| owner != account)
                    || matches!(&duration, SharingDuration::RoomSession { account: owner, .. } if owner != &account)
                    || matches!(&recipient, Recipient::MatrixRoom { account: owner, .. } if owner != &account)
                { return Err("The account changed. Review this sharing rule again.".into()); }
                a2app_core::information_flow::grant_sharing(source, recipient, reader, duration)
            })();
            if let Err(error) = checked {
                enqueue_popup_notification(error, PopupKind::Error, Some(6.0));
            } else if let Ok(account) = super::information_flow::account() {
                a2app_core::protection_audit::record_policy_change(&account);
            }
            ui.redraw(cx);
        }
        A2AppOp::RevokeFlowSharing(id) => {
            let grant = match a2app_core::information_flow::sharing_grants() {
                Ok(grants) => grants.into_iter().find(|grant| grant.id == id),
                Err(error) => { enqueue_popup_notification(error, PopupKind::Error, Some(6.0)); return; }
            };
            let Some(grant) = grant else { return };
            let belongs = super::information_flow::account().is_ok_and(|account| match &grant.source {
                a2app_core::information_flow::Source::Account { account: owner }
                | a2app_core::information_flow::Source::Room { account: owner, .. }
                | a2app_core::information_flow::Source::RoomDirectory { account: owner } => owner == &account,
                a2app_core::information_flow::Source::UnknownPrivate => false,
            });
            if !belongs { return; }
            let revoked = match a2app_core::information_flow::revoke_sharing(id) {
                Ok(revoked) => revoked,
                Err(error) => { enqueue_popup_notification(error, PopupKind::Error, Some(6.0)); return; }
            };
            if revoked && let Ok(account) = super::information_flow::account() { a2app_core::protection_audit::record_policy_change(&account); }
            if revoked { match grant.reader {
                a2app_core::information_flow::ReaderScope::AllReaders => stop_private_contexts(cx, ui),
                a2app_core::information_flow::ReaderScope::App { account, app } => {
                    if super::information_flow::account().is_ok_and(|current| current == account) {
                        permission_lifecycle::retire_after_revocation(cx, ui, &app);
                    }
                }
                a2app_core::information_flow::ReaderScope::Context(context) => retire_flow_context(cx, ui, &context),
            } }
            ui.redraw(cx);
        }
        A2AppOp::GrantFlowAuthority { context, action, session, expected_influences, expected_epoch } => {
            let result = super::information_flow::current_context(&context)
                .and_then(|()| a2app_core::information_flow::grant_authority_for_activation(&context, action, session, &expected_influences, expected_epoch));
            if let Err(error) = result {
                enqueue_popup_notification(error, PopupKind::Error, Some(6.0));
            } else {
                a2app_core::protection_audit::record_policy_change(context.account());
            }
            ui.redraw(cx);
        }
        A2AppOp::GrantExactFlowAuthority { context, request_id, expected_influences, expected_epoch } => {
            let result = super::information_flow::current_context(&context)
                .and_then(|()| a2app_core::information_flow::grant_exact_action_for_activation(&context, request_id, &expected_influences, expected_epoch));
            if let Err(error) = result {
                enqueue_popup_notification(error, PopupKind::Error, Some(6.0));
            } else {
                a2app_core::protection_audit::record_policy_change(context.account());
            }
            ui.redraw(cx);
        }
        A2AppOp::RevokeFlowAuthority(id) => {
            let context = match a2app_core::information_flow::authorities() {
                Ok(grants) => grants.into_iter().find(|grant| grant.id == id).map(|grant| grant.context),
                Err(error) => { enqueue_popup_notification(error, PopupKind::Error, Some(6.0)); return; }
            };
            let Some(context) = context else { return };
            if !super::information_flow::account().is_ok_and(|account| context.account() == account) { return; }
            let revoked = match a2app_core::information_flow::revoke_authority(id) {
                Ok(revoked) => revoked,
                Err(error) => { enqueue_popup_notification(error, PopupKind::Error, Some(6.0)); return; }
            };
            if revoked && let Ok(account) = super::information_flow::account() { a2app_core::protection_audit::record_policy_change(&account); }
            if revoked { retire_flow_context(cx, ui, &context); }
            ui.redraw(cx);
        }
        A2AppOp::RevokeEffectAuthority(id) => {
            let context = match a2app_core::information_flow::effect_authorities() {
                Ok(grants) => grants.into_iter().find(|grant| grant.id == id).map(|grant| grant.context),
                Err(error) => { enqueue_popup_notification(error, PopupKind::Error, Some(5.0)); return; }
            };
            let Some(context) = context else { return };
            if !super::information_flow::account().is_ok_and(|account| context.account() == account) { return; }
            match a2app_core::information_flow::revoke_effect_authority(id) {
                Ok(revoked) => {
                    if revoked && let Ok(account) = super::information_flow::account() { a2app_core::protection_audit::record_policy_change(&account); }
                    if revoked { retire_flow_context(cx, ui, &context); }
                    publish_grants(cx);
                    ui.redraw(cx);
                }
                Err(error) => enqueue_popup_notification(error, PopupKind::Error, Some(5.0)),
            }
        }
        A2AppOp::RoomClosed(room_id) => {
            super::background::room_closed(cx, room_id.as_str());
            if let Ok(account) = super::information_flow::account() {
                if let Err(error) = a2app_core::information_flow::close_room_session(&account, room_id.as_str()) {
                    enqueue_popup_notification(error, PopupKind::Error, Some(6.0));
                }
            }
            with_a2app(|state| { state.permissions.clear_room_session(room_id.as_str()); });
            #[cfg(unix)]
            {
                abort_ai_room_work(cx, ui, &room_id);
            }
            refresh_permission_policy(cx, ui);
        }
        A2AppOp::Unrestrict(app_id) => {
            with_a2app(|state| {
                state.permissions.unrestrict(&app_id);
                state.perms_dirty = true;
            });
            publish_grants(cx);
            enqueue_popup_notification("The app may run again.", PopupKind::Success, Some(3.0));
            ui.redraw(cx);
        }
        A2AppOp::StartGeneration { request, room_id } => start_generation(cx, ui, request, room_id, None),
        A2AppOp::StartCreate { request, room_id } => start_generation(cx, ui, request, room_id, Some(Intent::Create)),
        A2AppOp::StartModify { app_id, request } => start_generation(cx, ui, request, None, Some(Intent::Modify(app_id))),
        A2AppOp::CancelGeneration => {
            with_a2app(|state| {
                // Dropping the Generation kills the agent child process.
                state.generation = None;
                state.pending_generated = None;
                state.generation_context = None; state.generation_epoch = None;
                state.failed_request = None;
                state.console.review_context = None;
                state.console.status = String::from("Cancelled.");
            });
            // A session's launch_splash_app tool call may be waiting on this
            // generation; tell it the build was cancelled rather than leave
            // its serve thread parked forever.
            #[cfg(unix)]
            resolve_session_generation(cx, ui, None, Err(String::from("The generation was cancelled.")));
            ui.redraw(cx);
        }
        A2AppOp::RetryGeneration => {
            if let Some(pending) = with_a2app(|state| state.pending_generated.take()).flatten() {
                finish_generated_app(cx, ui, pending);
                return;
            }
            let retry = with_a2app(|state| state.failed_request.take().map(|request| (request, state.create_room.clone()))).flatten();
            if let Some((request, room)) = retry {
                start_generation(cx, ui, request, room, None);
            }
        }
        A2AppOp::NewPrompt => {
            with_a2app(|state| {
                state.console = GenConsole::default();
                state.failed_request = None;
                state.pending_generated = None;
            });
            ui.redraw(cx);
        }
        A2AppOp::ShareToRoom { app_id, room_id } => {
            if !room_policy_allows(room_id.as_str(), RoomAccess::Write) {
                enqueue_popup_notification(
                    format!("Sharing an app into a room is blocked: {MATRIX_WRITE_OFF_MSG}."),
                    PopupKind::Warning, Some(5.0),
                );
                return;
            }
            let Some(Some(manifest)) = with_a2app(|state| state.registry.get(&app_id).cloned()) else { return };
            let bundle_json = match bundle::try_to_text(&manifest) {
                Ok(text) => text,
                Err(error) => { enqueue_popup_notification(format!("Sharing failed: {error}"), PopupKind::Error, Some(5.0)); return; }
            };
            submit_async_request(MatrixRequest::A2App(A2AppMatrixRequest::ShareApp {
                room_id,
                bundle_json,
                app_name: manifest.name.clone(),
            }));
        }
        #[cfg(unix)]
        A2AppOp::AiRoomPanel(room_id) => {
            open_ai_room_panel(cx, ui, &room_id);
        }
        #[cfg(unix)]
        A2AppOp::AbortAiRoom(room_id) => {
            abort_ai_room_work(cx, ui, &room_id);
        }
    }
}

fn ensure_source_owner(source: &a2app_core::information_flow::Source) -> Result<(), String> {
    use a2app_core::information_flow::Source;
    let owner = match source {
        Source::Account { account } | Source::Room { account, .. } | Source::RoomDirectory { account } => account,
        Source::UnknownPrivate => return Err("Unknown private data cannot be released.".into()),
    };
    if super::information_flow::account()? == *owner { Ok(()) }
    else { Err("Switch to the source's account before changing its sharing rules.".into()) }
}

/// Reads a cached display name while keeping the Matrix ID as stable attribution.
fn current_version_actor(cx: &mut Cx) -> Option<VersionActor> {
    let user_id = crate::sliding_sync::current_user_id()?;
    let display_name = crate::profile::user_profile_cache::with_user_profile(
        cx, user_id.clone(), None, false, |profile, _| profile.username.clone(),
    ).flatten();
    Some(VersionActor { user_id: user_id.to_string(), display_name })
}

fn index_room_imports(registry: &AppRegistry) -> HashMap<(String, String, String), MiniAppId> {
    let mut index = HashMap::new();
    for app in registry.iter() {
        for version in persistence::list_versions(&app.id) {
            index_room_import(&mut index, &app.id, &version);
        }
    }
    index
}

fn index_room_import(index: &mut HashMap<(String, String, String), MiniAppId>, app_id: &str, version: &versions::AppVersion) {
    if version.imported { return; }
    let Some(actor) = &version.actor else { return };
    if let Some(AcquisitionSource::RoomAttachment { room_id, event_id, media_uri, .. }) = &version.acquired_from {
        for transfer in event_id.iter().chain(media_uri.iter()) {
            index.insert((actor.user_id.clone(), room_id.clone(), transfer.clone()), app_id.into());
        }
    }
}

/// The local copy added from this room event or file, including after restart.
pub fn imported_room_app(room_id: &RoomId, event_id: Option<&matrix_sdk::ruma::EventId>, media_uri: &str) -> Option<MiniAppManifest> {
    let account = crate::sliding_sync::current_user_id()?.to_string();
    with_a2app(|state| {
        event_id.map(|id| id.as_str()).into_iter().chain(std::iter::once(media_uri))
            .find_map(|transfer| state.imported_room_apps.get(&(account.clone(), room_id.to_string(), transfer.into()))
                .and_then(|id| state.registry.get(id)).cloned())
    }).flatten()
}

pub fn is_room_import_pending(media_uri: &str) -> bool {
    with_a2app(|state| state.room_imports.contains_key(media_uri)).unwrap_or(false)
}

fn start_room_import(
    cx: &mut Cx, ui: &WidgetRef,
    media_source: matrix_sdk::ruma::events::room::MediaSource, filename: String, size: Option<u64>,
    room_id: OwnedRoomId, event_id: Option<OwnedEventId>, sender_id: String, sender_name: String, shared_at_unix: Option<u64>,
) {
    use crate::shared::attachment_download::{media_source_mxc, MediaDownloadResult};
    let media_uri = media_source_mxc(&media_source).to_string();
    if imported_room_app(&room_id, event_id.as_deref(), &media_uri).is_some() {
        enqueue_popup_notification("This mini-app is already in your mini-apps.", PopupKind::Info, Some(3.0));
        ui.redraw(cx);
        return;
    }
    if size.is_some_and(|bytes| bytes > bundle::MAX_BUNDLE_BYTES as u64) {
        enqueue_popup_notification("That mini-app file is too large.", PopupKind::Error, Some(5.0));
        return;
    }
    let Some(account) = crate::sliding_sync::current_user_id().map(|id| id.to_string()) else {
        enqueue_popup_notification("Sign in before adding a shared mini-app.", PopupKind::Error, Some(5.0));
        return;
    };
    let actor = current_version_actor(cx);
    let source = AcquisitionSource::RoomAttachment {
        room_id: room_id.to_string(), event_id: event_id.map(|id| id.to_string()),
        media_uri: Some(media_uri.clone()), file_name: filename.clone(), shared_at_unix,
        sender: Some(VersionActor { user_id: sender_id, display_name: Some(sender_name) }),
    };
    let request_id = with_a2app(|state| {
        if state.room_imports.contains_key(&media_uri) { return None; }
        state.next_room_import = state.next_room_import.checked_add(1)?;
        state.room_imports.insert(media_uri.clone(), state.next_room_import);
        Some(state.next_room_import)
    }).flatten();
    let Some(request_id) = request_id else { return };
    submit_async_request(MatrixRequest::DownloadMedia {
        media_source, filename,
        on_download_result: Box::new(move |result| {
            let text = match result {
                MediaDownloadResult::Downloaded(bytes) if bytes.len() <= bundle::MAX_BUNDLE_BYTES =>
                    String::from_utf8(bytes).map_err(|_| "That mini-app file is not UTF-8 text.".into()),
                MediaDownloadResult::Downloaded(_) => Err("That mini-app file is too large.".into()),
                MediaDownloadResult::Failed(error) => Err(format!("Couldn't download the mini-app: {error}")),
                MediaDownloadResult::Cancelled => Err("The mini-app download was cancelled. Try adding it again.".into()),
            };
            Cx::post_action(A2AppOp::RoomAttachmentDownloaded { request_id, media_uri, account, actor, source, text });
        }),
    });
    ui.redraw(cx);
}

fn install_import(
    cx: &mut Cx, ui: &WidgetRef, parsed: Result<bundle::ImportedBundle, String>,
    source: AcquisitionSource, actor: Option<VersionActor>,
) {
    let installed = parsed.and_then(|imported| {
        with_a2app(|state| {
            let mut manifest = imported.manifest;
            // New identity never overwrites another app or its provenance.
            let taken: Vec<MiniAppId> = state.registry.iter().map(|a| a.id.clone()).collect();
            let suggested_id = manifest.id.clone();
            manifest.id = unique_import_id(&suggested_id, &taken);
            let context = super::information_flow::app_context(&manifest.id, None)?;
            a2app_core::information_flow::register_context(&context)?;
            // Selecting a unique installed id also depends on account inventory.
            let mut sources = vec![super::information_flow::account_source(&context)];
            if let AcquisitionSource::RoomAttachment { room_id, .. } = &source {
                sources.push(super::information_flow::room_source(&context, room_id));
            }
            a2app_core::information_flow::add_sources(&context, sources)?;
            a2app_core::information_flow::add_influences(&context, [a2app_core::information_flow::Influence::MiniApp {
                account: super::information_flow::context_account(&context).into(), app: manifest.id.clone(),
            }])?;
            a2app_core::information_flow::record_app_code_from(&manifest.id, &context)?;
            persistence::import_history(&mut manifest, &imported.history, imported.current_version.as_deref())
                .map_err(|error| error.to_string())?;
            let (origin, note) = match &source {
                AcquisitionSource::Clipboard => (VersionOrigin::Import, "Added from clipboard or pasted text"),
                AcquisitionSource::File { .. } => (VersionOrigin::Import, "Imported from a file"),
                AcquisitionSource::RoomAttachment { .. } => (VersionOrigin::Import, "Added from a room message"),
                _ => (VersionOrigin::Import, "Imported"),
            };
            commit_version(&mut manifest, origin, note, actor, Some(source))?;
            persistence::save_user_app(&manifest).map_err(|error| error.to_string())?;
            if let Some(stamp) = manifest.current_version.as_deref()
                && let Some((version, _)) = persistence::load_version(&manifest.id, stamp)
            { index_room_import(&mut state.imported_room_apps, &manifest.id, &version); }
            state.registry.insert(manifest.clone());
            Ok(manifest)
        }).unwrap_or_else(|| Err("Mini-app state is unavailable.".into()))
    });
    match installed {
        Ok(manifest) => {
            publish_grants(cx);
            enqueue_popup_notification(
                format!("Added \"{}\" to your mini-apps.", manifest_name_for_popup(&manifest)),
                PopupKind::Success, Some(4.0),
            );
            ui.redraw(cx);
        }
        Err(e) => enqueue_popup_notification(format!("Import failed: {e}"), PopupKind::Error, Some(5.0)),
    }
}

fn manifest_flow_context(manifest: &MiniAppManifest) -> Result<a2app_core::information_flow::ContextId, String> {
    let context = super::information_flow::app_context(&manifest.id, None)?;
    a2app_core::information_flow::register_context_with_legacy_data(&context, super::information_flow::manifest_has_private_source(manifest))?;
    Ok(context)
}

/// Reading an app listing or source does not read its room compartments.
/// Fail-closed: an app whose code carries legacy/unknown provenance taints
/// the receiver. The metadata-only `list_apps` path does not call this.
fn join_manifest_code(manifest: &MiniAppManifest, receiver: &a2app_core::information_flow::ContextId) -> Result<(), String> {
    manifest_flow_context(manifest)?;
    a2app_core::information_flow::join_code_labels(&manifest.id, receiver)
}

fn manifest_name_for_popup(manifest: &MiniAppManifest) -> String {
    if manifest.name.is_empty() { manifest.id.clone() } else { manifest.name.clone() }
}

fn unique_import_id(base: &str, taken: &[MiniAppId]) -> MiniAppId {
    if !taken.iter().any(|t| t == base) && !persistence::has_app_files(base) {
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if !taken.contains(&candidate) && !persistence::has_app_files(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

// -----------------------------------------------------------------------
// Generation
// -----------------------------------------------------------------------

fn start_generation(
    cx: &mut Cx,
    ui: &WidgetRef,
    request: String,
    room_id: Option<OwnedRoomId>,
    intent: Option<Intent>,
) {
    if let Some(blocker) = a2app_agent::blocker() {
        enqueue_popup_notification(blocker.headline(), PopupKind::Warning, Some(6.0));
        return;
    }
    let mut failed = None;
    let actor = current_version_actor(cx);
    with_a2app(|state| {
        if state.generation.is_some() {
            enqueue_popup_notification("A generation is already running.", PopupKind::Warning, Some(3.0));
            return;
        }
        state.pending_generated = None;
        state.console.review_context = None;
        state.failed_request = Some(request.clone());
        let apps: Vec<(MiniAppId, String)> = state.registry.iter()
            .map(|a| (a.id.clone(), a.name.clone()))
            .collect();
        let taken: Vec<MiniAppId> = apps.iter().map(|(id, _)| id.clone()).collect();

        // An explicit intent wins; otherwise classify the request text.
        let refine_target = match intent.unwrap_or_else(|| a2app_agent::intent::classify(&request, &apps)) {
            Intent::Modify(id) => Some(id),
            Intent::Create => None,
        };

        let scope = match &room_id {
            Some(r) => A2AppScope::Room { room_id: r.to_string() },
            None => A2AppScope::Account,
        };
        state.create_room = room_id.clone();

        let status = match refine_target.as_ref().and_then(|id| state.registry.get(id)) {
            Some(target) => format!("Connecting to the agent to rewrite \"{}\"…", target.name),
            None => String::from("Connecting to the agent to create a new app…"),
        };
        let context = (|| -> Result<_, String> {
            let context = match room_id.as_ref() {
                Some(room) => super::information_flow::prepare_agent(room.as_str())?,
                None => {
                    let context = super::information_flow::app_context("__generation", None)?;
                    a2app_core::information_flow::register_context(&context)?;
                    a2app_core::information_flow::add_sources(&context, [super::information_flow::account_source(&context)])?;
                    context
                }
            };
            if let Some(base) = refine_target.as_ref().and_then(|id| state.registry.get(id)) {
                join_manifest_code(base, &context)?;
            }
            Ok(context)
        })();
        let context = match context {
            Ok(context) => context,
            Err(error) => {
                state.console.status = error.clone();
                state.console.active = true;
                enqueue_popup_notification(error.clone(), PopupKind::Error, Some(6.0));
                failed = Some(error);
                return;
            }
        };
        let generation = match refine_target.and_then(|id| state.registry.get(&id).cloned()) {
            Some(mut base) => {
                // The state being rewritten stays reachable as a version.
                if let Err(error) = archive_current(&mut base) {
                    state.console.status = error.clone();
                    state.console.active = true;
                    enqueue_popup_notification(error.clone(), PopupKind::Error, Some(6.0));
                    failed = Some(error);
                    return;
                }
                state.registry.insert(base.clone());
                Generation::start_refine(request.clone(), base, state.agent_prefs.clone(), Some(context.clone()))
            }
            None => Generation::start(request.clone(), taken, scope, state.agent_prefs.clone(), Some(context.clone())),
        };
        match generation {
            Ok(generation) => {
                state.generation_attribution = Some((actor, AcquisitionSource::Generated {
                    model: a2app_agent::model_transport::current_recipient(&state.agent_prefs).ok().map(|recipient| recipient.label),
                    room_id: room_id.as_ref().map(ToString::to_string),
                }));
                state.generation = Some(generation);
                state.generation_epoch = a2app_core::information_flow::context_epoch(&context).ok();
                state.generation_context = Some(context);
                state.console = GenConsole {
                    status,
                    lines: Vec::new(),
                    review_context: None,
                    active: true,
                    last_render: None,
                };
                state.failed_request = Some(request);
            }
            Err(e) => {
                state.console.failed(&e, Some(context));
                state.console.active = true;
                failed = Some(e.clone());
                enqueue_popup_notification(e, PopupKind::Error, Some(6.0));
            }
        }
    });
    #[cfg(unix)]
    if let Some(error) = failed {
        resolve_session_generation(cx, ui, None, Err(format!("The build could not start: {error}")));
    }
    ui.redraw(cx);
}

fn advance_generation(cx: &mut Cx, ui: &WidgetRef) {
    enum Done {
        Ready { manifest: Box<MiniAppManifest>, refine_of: Option<MiniAppId> },
        Failed(String),
    }
    let done = with_a2app(|state| {
        let generation = state.generation.as_mut()?;
        // The stall watchdog: a live-but-silent agent (hung provider, dead
        // network) produces no events, so it would otherwise leave the UI
        // busy forever — and a session whose `launch_splash_app` tool call is
        // waiting on this build would stay parked on an unanswered call. Fail
        // the run so it resolves (the room's AI then reports the failure).
        if generation.is_stalled() {
            refresh_console(state, true);
            let reason = "The agent went silent; the run was stopped.".to_string();
            state.console.status = reason.clone();
            return Some(Done::Failed(reason));
        }
        match generation.advance(cx) {
            GenOutcome::Working => {
                refresh_console(state, false);
                None
            }
            GenOutcome::Ready { manifest, refine_of } => {
                refresh_console(state, true);
                Some(Done::Ready { manifest, refine_of })
            }
            GenOutcome::Failed(reason) => {
                refresh_console(state, true);
                let context = state.generation_context.clone().filter(|context|
                    state.generation_epoch.is_some_and(|epoch|
                        a2app_core::information_flow::ensure_context_epoch(context, epoch).is_ok())
                        && super::information_flow::current_context(context).is_ok());
                state.console.failed(&reason, context);
                Some(Done::Failed(reason))
            }
        }
    }).flatten();

    match done {
        Some(Done::Ready { mut manifest, refine_of }) => {
            let pending = with_a2app(|state| -> Result<PendingGeneratedApp, String> {
                if refine_of.is_none() && let Some(room) = state.create_room.take() {
                    manifest.scope = A2AppScope::Room { room_id: room.to_string() };
                }
                let context = state.generation_context.clone().ok_or("Missing generated-source provenance.")?;
                let epoch = state.generation_epoch.ok_or("Missing generation activation.")?;
                let request = state.generation.as_ref().map(|g| g.request().to_string()).unwrap_or_default();
                #[cfg(unix)]
                let action_target = state.ai_generation_room.as_ref().map(ToString::to_string);
                #[cfg(not(unix))]
                let action_target = None;
                Ok(PendingGeneratedApp { manifest, refine_of, context, epoch, request, attribution: state.generation_attribution.take(), action_target, review_id: None, effect_review_id: None })
            }).unwrap_or_else(|| Err("Mini-app state is unavailable.".into()));
            match pending {
                Ok(pending) => finish_generated_app(cx, ui, pending),
                Err(error) => {
                    with_a2app(|state| { state.generation = None; state.generation_context = None; state.generation_epoch = None; state.pending_generated = None; });
                    #[cfg(unix)]
                    resolve_session_generation(cx, ui, None, Err(error.clone()));
                    enqueue_popup_notification(error, PopupKind::Error, Some(6.0));
                }
            }
        }
        Some(Done::Failed(reason)) => {
            with_a2app(|state| {
                state.generation = None;
                state.generation_context = None; state.generation_epoch = None;
            });
            #[cfg(unix)]
            resolve_session_generation(cx, ui, None, Err(format!("The build failed: {reason}")));
            ui.redraw(cx);
        }
        None => {}
    }
}

/// Install the captured build after its permission decision, without regeneration.
fn finish_generated_app(cx: &mut Cx, ui: &WidgetRef, mut pending: PendingGeneratedApp) {
    let payload = serde_json::to_value(&pending.manifest).map_err(|_| "Cannot review the generated mini-app.".to_owned());
    let action = pending.install_action();
    let review = payload.as_ref().map_err(|error| error.clone()).and_then(|payload| {
        super::information_flow::current_context(&pending.context)?;
        a2app_core::information_flow::ensure_context_epoch(&pending.context, pending.epoch)?;
        let valid = with_a2app(|state| {
            state.generation_context.as_ref() == Some(&pending.context) && state.generation_epoch == Some(pending.epoch)
                && (pending.refine_of.is_some() || (state.registry.get(&pending.manifest.id).is_none()
                    && !persistence::has_app_files(&pending.manifest.id)))
        }).unwrap_or(false);
        if !valid { return Err("This completed build was cancelled or another app now uses its ID. Start a new generation.".into()); }
        a2app_core::information_flow::prepare_effect_for_activation(&pending.context, pending.epoch, None, Some(&action), payload)
    });
    if let Ok(review) = &review && !review.allowed {
        if pending.can_retain() {
            pending.effect_review_id = Some(review.id);
            let review = review.clone();
            with_a2app(|state| {
                state.generation = None;
                state.failed_request = None;
                state.console.status = "The app is ready. Choose whether to install it in the permission popup.".into();
                state.pending_generated = Some(pending);
            });
            queue_flow_prompt(cx, ui, FlowContinuation::Generated { review });
            ui.redraw(cx);
            return;
        }
        let _ = a2app_core::information_flow::cancel_effect(review.id);
    }
    let result = review.and_then(|review| {
        if !review.allowed { return Err("The generated app is too large to retain for review. Ask for a smaller app.".into()); }
        let payload = payload?;
        with_a2app(|state| -> Result<(), String> {
            super::information_flow::current_context(&pending.context)?;
            a2app_core::information_flow::ensure_context_epoch(&pending.context, pending.epoch)?;
            if pending.refine_of.is_none() && (state.registry.get(&pending.manifest.id).is_some()
                || persistence::has_app_files(&pending.manifest.id)) {
                return Err("Another saved app now uses this generated app's ID. Start a new generation.".into());
            }
            a2app_core::information_flow::commit_effect_for_activation(&pending.context, pending.epoch, None, Some(&action), &payload)?;
            // Provenance reaches code before any version or source is persisted.
            a2app_core::information_flow::add_sources(&pending.context, [super::information_flow::account_source(&pending.context)])?;
            let room = match &pending.manifest.scope { A2AppScope::Room { room_id } => Some(room_id.as_str()), A2AppScope::Account => None };
            let to = super::information_flow::app_context(&pending.manifest.id, room)?;
            let legacy = state.registry.get(&pending.manifest.id).is_some_and(super::information_flow::manifest_has_private_source);
            a2app_core::information_flow::register_context_with_legacy_data(&to, legacy)?;
            a2app_core::information_flow::transfer(&pending.context, &to)?;
            a2app_core::information_flow::record_app_code_from(&pending.manifest.id, &pending.context)?;
            let (actor, source) = pending.attribution.clone().map(|(actor, source)| (actor, Some(source)))
                .unwrap_or((None, None));
            commit_version(&mut pending.manifest, VersionOrigin::Ai, &pending.request, actor, source)?;
            persistence::save_user_app(&pending.manifest).map_err(|_| "Could not save the reviewed mini-app.".to_string())?;
            state.clear_adopted_builtin_update(&pending.manifest);
            state.registry.insert((*pending.manifest).clone());
            state.generation = None;
            state.generation_context = None; state.generation_epoch = None;
            state.failed_request = None;
            state.console.status = if pending.refine_of.is_some() { format!("Updated \"{}\".", pending.manifest.name) }
                else { format!("Created \"{}\".", pending.manifest.name) };
            state.registry_dirty = true;
            Ok(())
        }).unwrap_or_else(|| Err("Mini-app state is unavailable.".into()))
    });
    if let Err(reason) = result {
        with_a2app(|state| {
            state.generation = None;
            state.generation_context = None; state.generation_epoch = None;
            state.failed_request = None;
            state.console.status = format!("The app was not installed: {reason}");
        });
        #[cfg(unix)]
        resolve_session_generation(cx, ui, None, Err(format!("The build could not be installed: {reason}")));
        enqueue_popup_notification(reason, PopupKind::Error, Some(8.0));
        ui.redraw(cx);
        return;
    }
    publish_grants(cx);
    let was_running = stop_for_restart(cx, ui, &pending.manifest);
    cx.action(A2AppRuntimeAction::VersionsChanged(pending.manifest.id.clone()));
    #[cfg(unix)]
    {
        let summary = serde_json::json!({ "app_id": pending.manifest.id, "name": pending.manifest.name, "status": "installed_and_running" });
        resolve_session_generation(cx, ui, Some(&pending.manifest), Ok(summary.to_string()));
    }
    enqueue_popup_notification(reopen_hint(format!("Mini-app \"{}\" is ready.", pending.manifest.name), was_running), PopupKind::Success, Some(5.0));
    ui.redraw(cx);
}

/// Rebuilds the console's line list from the generation's trail + transcript,
/// throttled so a fast-streaming agent doesn't re-split per token.
fn refresh_console(state: &mut A2AppState, force: bool) {
    let now = Instant::now();
    if !force
        && state.console.last_render.is_some_and(|last| now.duration_since(last) < CONSOLE_REPAINT)
    {
        return;
    }
    let Some(generation) = state.generation.as_ref() else { return };
    state.console.status = generation.status_line();
    let mut lines: Vec<String> = generation.activity().to_vec();
    for line in generation.transcript().lines() {
        lines.push(line.to_string());
    }
    state.console.lines = lines;
    state.console.last_render = Some(now);
    SignalToUI::set_ui_signal();
}

// -----------------------------------------------------------------------
// Broker + permission prompts
// -----------------------------------------------------------------------

fn compartment_storage_path(heap: usize) -> Result<PathBuf, String> {
    let context = super::information_flow::context_for_heap(heap)?;
    a2app_core::information_flow::context_storage_path(&context)
}

fn process_broker(cx: &mut Cx, ui: &WidgetRef) {
    let asks = with_a2app(|state| {
        let A2AppState { broker, registry, permissions, foreground_app, .. } = state;
        let is_docked = |app_id: &str| instances::is_foreground(app_id);
        let desktop_view = effective_is_desktop(cx);
        let rooms = cx.has_global::<RoomsListRef>().then(|| cx.get_global::<RoomsListRef>().clone());
        let room_name = |id: &str| room_display_name(rooms.as_ref()?, id);
        broker.process(cx, BrokerCtx {
            registry,
            permissions,
            foreground_app: foreground_app.as_deref(),
            is_docked: &is_docked,
            is_running: &instances::is_running,
            pane_state: &instances::pane_state,
            storage_path: &compartment_storage_path,
            room_name: &room_name,
            permission_target_room: Some(&permission_target_room),
            desktop_view,
            check_flow: &super::information_flow::check_request,
            check_response: &super::information_flow::check_response,
        })
    }).unwrap_or_default();

    let had_asks = !asks.is_empty();
    for ask in asks {
        apply_broker_ask(cx, ui, ask);
    }
    if had_asks {
        ui.redraw(cx);
    }
}

fn apply_broker_ask(cx: &mut Cx, ui: &WidgetRef, ask: BrokerAsk) {
    match ask {
        BrokerAsk::PermissionBatch { app_id, request, perms } => permission_batch::queue_setup(cx, ui, app_id, request, perms),
        BrokerAsk::EnableWrites { app_id, perm, request } => {
            queue_permission_prompt(cx, ui, app_id, perm, ParkedRequest::Bridge(Some(request)), None);
        }
        BrokerAsk::FlowReview { request, review } => {
            let allow_once = !request.service.starts_with("matrix.") && request.service != "network.http";
            queue_flow_prompt(cx, ui, FlowContinuation::Bridge { request, review, allow_once, permission: None });
        }
        BrokerAsk::Prompt { app_id, perm, request, tool } => {
            let grant = tool.map(|t| PromptToolGrant {
                full_name: t.full_name,
                name: t.name,
                description: t.description,
                args: t.args,
                content_hash: t.content_hash,
            });
            queue_permission_prompt(cx, ui, app_id, perm, ParkedRequest::Bridge(request), grant);
        }
        BrokerAsk::Notify { app_id, summary } => {
            let name = with_a2app(|state| {
                state.registry.get(&app_id).map(|a| a.name.clone())
            }).flatten().unwrap_or_else(|| app_id.clone());
            enqueue_popup_notification(format!("{name}: {summary}"), PopupKind::Info, Some(5.0));
        }
        BrokerAsk::Used { app_id, perm } => {
            with_a2app(|state| {
                state.permissions.record_access(&app_id, perm, a2app_core::versions::now_unix());
                state.perms_dirty = true;
            });
        }
        BrokerAsk::Restrict { app_id, reason } => {
            with_a2app(|state| {
                let refusals = state.broker.refusal_count(&app_id);
                state.permissions.restrict(&app_id, &reason, a2app_core::versions::now_unix(), refusals);
                state.perms_dirty = true;
                state.foreground_app.take_if(|f| *f == app_id);
            });
            stop_app_everywhere(cx, ui, &app_id);
            publish_grants(cx);
            enqueue_popup_notification(
                format!("A mini-app was stopped for flooding the host with requests ({reason}). You can let it run again from its app info."),
                PopupKind::Warning, Some(8.0),
            );
            ui.redraw(cx);
        }
        BrokerAsk::IpcDeliver { reply, from, from_heap, to, data_json, receipt } => {
            let delivered = instances::deliver_ipc(cx, from_heap, &from, &to, &data_json);
            if receipt {
                let body = format!("{{\"delivered\":{delivered}}}");
                services::respond(cx, reply, Ok(&body));
            }
        }
        BrokerAsk::Network { reply, app_id, room, args, consent } => {
            let prepared = super::information_flow::context_for_heap(reply.heap_key)
                .and_then(|context| {
                    let epoch = a2app_core::information_flow::context_epoch(&context)?;
                    super::network::Request::parse(&args).map(|request| (context, epoch, request))
                });
            let lifetime = instances::lifetime_of_heap(reply.heap_key);
            if lifetime.is_none() { return services::respond(cx, reply, Err("The requesting app stopped.")); }
            match prepared {
                Ok((context, flow_epoch, request)) => crate::sliding_sync::spawn_async_task(async move {
                    let result = super::network::run(request, context.clone(), app_id, room, *consent, lifetime).await;
                    Cx::post_action(HostNetworkResult { reply, context, flow_epoch, result });
                }),
                Err(error) => services::respond(cx, reply, Err(&error)),
            }
        }
        BrokerAsk::Matrix { reply, app_id, room, call, capability, consent, args } => {
            let flow_context = match super::information_flow::context_for_heap(reply.heap_key) {
                Ok(context) => context,
                Err(error) => return services::respond(cx, reply, Err(&error)),
            };
            let origin = room.clone();
            let room = room.and_then(|r| OwnedRoomId::try_from(r.as_str()).ok());
            match matrix::request_for(call, room, reply) {
                Ok(request) => submit_async_request(MatrixRequest::A2App(request.authorized(app_id, capability, origin, consent, flow_context, args))),
                Err(e) => {
                    with_a2app(|state| state.broker.note(reply, &app_id));
                    services::respond(cx, reply, Err(e));
                }
            }
        }
        BrokerAsk::BackgroundComplete { reply, run_id, success } => {
            let result = super::background::complete(cx, reply.heap_key, run_id, success);
            services::respond(cx, reply, result.as_ref().map(|_| "{}").map_err(String::as_str));
            if result.is_ok() { super::background::retire_completed(cx, reply.heap_key); }
        }
        BrokerAsk::Subscribe { reply, app_id, heap_key, room, hook } => {
            let Ok(room_id) = room.as_deref().map(OwnedRoomId::try_from).transpose() else {
                return services::respond(cx, reply, Err("not a valid room id"));
            };
            if let Err(error) = super::information_flow::record_subscription(heap_key, hook) {
                return services::respond(cx, reply, Err(&error));
            }
            with_a2app(|state| {
                let sub = state.hook_subs.entry(heap_key).or_insert_with(|| HookSubscription {
                    app_id,
                    room_id,
                    hooks: HashSet::new(),
                });
                sub.hooks.insert(hook);
            });
            prune_hook_subs();
            services::respond(cx, reply, Ok("{\"subscribed\":true}"));
        }
        BrokerAsk::Unsubscribe { reply, heap_key, hook } => {
            with_a2app(|state| {
                match hook {
                    Some(hook) => {
                        if let Some(sub) = state.hook_subs.get_mut(&heap_key) {
                            sub.hooks.remove(hook);
                        }
                    }
                    None => { state.hook_subs.remove(&heap_key); }
                }
            });
            prune_hook_subs();
            services::respond(cx, reply, Ok("{}"));
        }
        BrokerAsk::HostQuery { reply, query } => {
            let data = match query {
                HostQuery::Prefs => prefs_json(cx),
                HostQuery::DeviceInfo => serde_json::json!({
                    "platform": services::PLATFORM,
                    "locale": std::env::var("LANG").ok(),
                    "time_zone": iana_time_zone::get_timezone().ok(),
                    "utc_offset_minutes": chrono::Local::now().offset().local_minus_utc() / 60,
                    "desktop_view": effective_is_desktop(cx),
                    "ui_zoom": cx.global::<AppPreferencesGlobal>().0.ui_zoom.0,
                }),
            };
            services::respond(cx, reply, Ok(&data.to_string()));
        }
        BrokerAsk::HostAction { reply, app_id, action } => {
            // Queued composer actions retain their requester until the room
            // checks its live authority. Other modal departures quit now.
            let queued_composer = matches!(action, HostAction::ComposerInsert { .. } | HostAction::ComposerReplyTo { .. });
            let leaves_modal = !matches!(action, HostAction::OpenApp { .. } | HostAction::Minimize | HostAction::Restore)
                && host_pane(cx, ui).active().is_some_and(|key| {
                    key.0 == app_id && instances::heap_of(&key) == Some(reply.heap_key)
                });
            match perform_host_action(cx, ui, reply.heap_key, action) {
                Ok(()) => services::respond(cx, reply, Ok("{}")),
                Err(e) => {
                    services::respond(cx, reply, Err(&e));
                    return;
                }
            }
            if leaves_modal {
                if queued_composer {
                    let key = host_pane(cx, ui).close_active(cx, true);
                    with_a2app(|state| {
                        state.foreground_app = None;
                        if let Some(pending) = state.room_action.as_mut() { pending.close_after = key; }
                    });
                    ui.modal(cx, ids!(mini_app_host_modal)).close(cx);
                    ui.redraw(cx);
                } else {
                    apply_op(cx, ui, A2AppOp::CloseHostPane);
                }
            }
        }
        #[cfg(unix)]
        BrokerAsk::McpRegisterTool { reply, request } => {
            register_app_tool(cx, reply, request);
        }
        #[cfg(unix)]
        BrokerAsk::McpUnregisterTool { reply, app_id, heap_key, full_name } => {
            unregister_app_tool(cx, reply, &app_id, heap_key, &full_name);
        }
        #[cfg(unix)]
        BrokerAsk::McpToolResult { reply, app_id, call_id, ok, text } => {
            complete_app_tool_call(cx, reply, &app_id, call_id, ok, &text);
        }
        // AI sessions are Unix-only today, so the tool bridge is too. The
        // broker still constructs these asks on other platforms, so answer
        // them rather than fail to compile.
        #[cfg(not(unix))]
        BrokerAsk::McpRegisterTool { reply, .. }
        | BrokerAsk::McpUnregisterTool { reply, .. }
        | BrokerAsk::McpToolResult { reply, .. } => {
            services::respond(cx, reply, Err("AI tools are not available on this platform"));
        }
    }
}

/// Installs a reviewed mini-app tool on the room's live agent session, then
/// answers the app with the namespaced name the model will see. A room with
/// no live session has nothing to register onto, so the app is told to retry.
#[cfg(unix)]
fn register_app_tool(cx: &mut Cx, reply: Reply, request: AppToolRequest) {
    let Some(room_id) = request
        .room
        .as_deref()
        .and_then(|r| OwnedRoomId::try_from(r).ok())
    else {
        return services::respond(
            cx,
            reply,
            Err("this app is not attached to a room with an AI session"),
        );
    };
    let flow_result = super::information_flow::context_for_heap(request.heap_key).and_then(|from| {
        let to = super::information_flow::prepare_agent(room_id.as_str())?;
        a2app_core::information_flow::transfer(&from, &to)
    });
    if let Err(error) = flow_result { return services::respond(cx, reply, Err(&error)); }
    let result = with_a2app(|state| {
        // A name already owned by another instance is a shadowing attempt.
        // The SAME instance may re-register (a change was re-reviewed, or it
        // is retrying from `on_permissions_changed`), which replaces in place.
        match state.app_tools.get(&request.full_name) {
            Some(existing) if existing.heap_key != request.heap_key => {
                return Err(String::from("a tool with that name is already registered"));
            }
            Some(_) => {}
            None => {
                let already = state
                    .app_tools
                    .values()
                    .filter(|reg| reg.heap_key == request.heap_key)
                    .count();
                if already >= a2app_core::services::MAX_APP_TOOLS_PER_INSTANCE {
                    return Err(String::from("this app has registered too many AI tools"));
                }
            }
        }
        if !state.ai_sessions.contains_key(&room_id) {
            return Err(String::from("this room has no live AI session right now"));
        }
        {
            let session = state.ai_sessions.get(&room_id).expect("checked above");
            session.register_miniapp_tool(
                request.full_name.clone(),
                request.description.clone(),
                request.schema.clone(),
            )?;
        }
        state.app_tools.insert(
            request.full_name.clone(),
            AppToolRegistration {
                full_name: request.full_name.clone(),
                raw_name: request.name.clone(),
                room_id: room_id.clone(),
                app_id: request.app_id.clone(),
                heap_key: request.heap_key,
                description: request.description.clone(),
                schema: request.schema.clone(),
                args: request.args.clone(),
            },
        );
        Ok(())
    })
    .unwrap_or_else(|| Err(String::from("app state unavailable")));
    match result {
        Ok(()) => services::respond(
            cx,
            reply,
            Ok(&serde_json::json!({ "tool": request.full_name }).to_string()),
        ),
        Err(e) => services::respond(cx, reply, Err(&e)),
    }
}

/// Transfers the latest app provenance before changing an agent's tools.
///
/// Tool availability is app-controlled output too: a removal or timeout can
/// depend on private data read after the original registration.
#[cfg(unix)]
fn transfer_app_tool_provenance(state: &A2AppState, app_id: &str, heap_key: usize, room: &OwnedRoomId) -> Result<(), String> {
    let live_context = instances::context_of_heap(heap_key).filter(|context| matches!(context,
        a2app_core::information_flow::ContextId::App { app, .. } if app == app_id));
    let from = if let Some(context) = live_context {
        super::information_flow::current_context(&context)?;
        context
    } else if let Some(manifest) = state.registry.get(app_id) {
        let context = super::information_flow::app_context(app_id, Some(room.as_str()))?;
        a2app_core::information_flow::register_context_with_legacy_data(&context, super::information_flow::manifest_has_private_source(manifest))?;
        context
    } else {
        // Uninstall removes the manifest, not the persistent provenance.
        let context = super::information_flow::app_context(app_id, Some(room.as_str()))?;
        a2app_core::information_flow::register_context_with_legacy_data(&context, true)?;
        context
    };
    let to = super::information_flow::prepare_agent(room.as_str())?;
    a2app_core::information_flow::transfer(&from, &to)
}

/// Withdraws one tool, but only for the instance that registered it.
#[cfg(unix)]
fn unregister_app_tool(
    cx: &mut Cx,
    reply: Reply,
    app_id: &str,
    heap_key: usize,
    full_name: &str,
) {
    let mut failed_rooms = Vec::new();
    let removed = with_a2app(|state| {
        let owner = state
            .app_tools
            .get(full_name)
            .map(|reg| (reg.heap_key, reg.app_id.clone(), reg.room_id.clone()));
        match owner {
            Some((reg_heap, reg_app, room)) if reg_heap == heap_key && reg_app == app_id => {
                if transfer_app_tool_provenance(state, app_id, heap_key, &room).is_err() {
                    state.ai_sessions.remove(&room);
                    failed_rooms.push(room.clone());
                }
                state.app_tools.remove(full_name);
                if let Some(session) = state.ai_sessions.get(&room) {
                    session.unregister_miniapp_tool(full_name);
                }
                true
            }
            _ => false,
        }
    })
    .unwrap_or(false);
    for room in failed_rooms { stop_ai_session(&room); }
    if removed {
        services::respond(cx, reply, Ok("{}"));
    } else {
        services::respond(cx, reply, Err("no such tool registered by this app"));
    }
}

/// Hands an app's answer to the serve thread parked on the call, and mirrors
/// the outcome on the model-facing turn card. A call id the app never got (or
/// one already swept by the timeout) is refused rather than misrouted.
#[cfg(unix)]
fn complete_app_tool_call(
    cx: &mut Cx,
    reply: Reply,
    app_id: &str,
    call_id: u64,
    ok: bool,
    text: &str,
) {
    let pending = with_a2app(|state| match state.app_tool_calls.get(&call_id) {
        Some(p) if p.app_id == app_id && p.heap_key == reply.heap_key => state.app_tool_calls.remove(&call_id),
        _ => None,
    })
    .flatten();
    let Some(pending) = pending else {
        return services::respond(cx, reply, Err("no matching tool call for this app"));
    };
    pending.audit.finish(ok);
    let transfer = super::information_flow::context_for_heap(reply.heap_key).and_then(|from| {
        let to = super::information_flow::prepare_agent(pending.room_id.as_str())?;
        a2app_core::information_flow::transfer(&from, &to)
    });
    if let Err(error) = transfer {
        let _ = pending.answer.send(Err(error.clone()));
        return services::respond(cx, reply, Err(&error));
    }
    let detail = finish_ai_tool_call(&pending.room_id, &pending.display_name, ok, text);
    push_ai_tool_receipt(&pending.room_id, &pending.display_name, detail, ok, text);
    let _ = if ok {
        pending.answer.send(Ok(text.to_string()))
    } else {
        pending.answer.send(Err(text.to_string()))
    };
    services::respond(cx, reply, Ok("{}"));
}

/// Applies one nav.* / composer.* call. Global moves happen right here;
/// room-scoped ones open the room and park the action for its RoomScreen.
/// A room's display name, once the rooms list exists (it does not before
/// the home screen is up).
pub fn room_display_name(rooms: &RoomsListRef, room_id: &str) -> Option<String> {
    let room_id = OwnedRoomId::try_from(room_id).ok()?;
    rooms.get_room_name(&room_id).map(|name| name.display().into_owned())
}

/// The name of a room the user is in. Anything else would open a join
/// dialog on an app's say-so.
fn joined_room_name(cx: &mut Cx, room_id: &OwnedRoomId) -> Result<RoomNameId, String> {
    let rooms = cx.get_global::<RoomsListRef>();
    if rooms.get_room_state(room_id) != Some(RoomState::Joined) {
        return Err(String::from("not a room you're in"));
    }
    Ok(rooms.get_room_name(room_id).unwrap_or_else(|| RoomNameId::empty(room_id.clone())))
}

/// Parks `action` for `room_id`'s RoomScreen and navigates there. The
/// caller may be on the Mini Apps tab; the room lives on Home.
fn queue_room_action(cx: &mut Cx, room_id: OwnedRoomId, action: RoomAction) -> Result<(), String> {
    queue_authorized_room_action(cx, room_id, action, None)
}

fn queue_authorized_room_action(
    cx: &mut Cx,
    room_id: OwnedRoomId,
    action: RoomAction,
    authorization: Option<matrix::policy::MatrixAuthorization>,
) -> Result<(), String> {
    let destination_room = BasicRoomDetails::Name(joined_room_name(cx, &room_id)?);
    let previous = with_a2app(|state| {
        let mut pending = PendingRoomAction {
            room_id: room_id.clone(),
            action,
            since: Instant::now(),
            authorization,
            close_after: None,
        };
        if let Some(previous) = state.room_action.as_mut() { pending.inherit_requester(previous); }
        state.room_action.replace(pending)
    }).flatten();
    if let Some(previous) = previous { previous.close_requester(cx); }
    cx.action(NavigationBarAction::GoToHome);
    cx.action(AppStateAction::NavigateToRoom { room_to_close: None, destination_room });
    cx.action(A2AppRoomAction::Pending { room_id });
    Ok(())
}

fn queue_composer_action(cx: &mut Cx, heap: usize, room_id: OwnedRoomId, action: RoomAction, capability: &str) -> Result<(), String> {
    let context = super::information_flow::context_for_heap(heap)?;
    let (app, origin_room) = match &context {
        a2app_core::information_flow::ContextId::App { app, room, .. } => (app, room.as_deref()),
        a2app_core::information_flow::ContextId::PublicApp { app, .. } => (app, None),
        _ => return Err("Invalid mini-app context.".into()),
    };
    let authorization = with_a2app(|state| matrix::policy::MatrixAuthorization::new(app, capability, origin_room, &state.permissions))
        .ok_or("Mini Apps is unavailable.")?.with_flow(context);
    authorization.check_flow(Some(room_id.as_str()), RoomAccess::Write)?;
    queue_authorized_room_action(cx, room_id, action, Some(authorization))
}

/// Brings `room_id` forward (navigating to it if needed) and, once its
/// timeline is showing, scrolls to `event_id` — how a click on a permalink to
/// a message in ANOTHER room resolves (e.g. an AI reply that points at
/// content the user asked about, or any chat link to a message elsewhere).
/// Same-room links never come here; they jump directly on the RoomScreen.
/// `Err` when the room isn't a joined room Robrix can open.
pub fn open_event_in_room(cx: &mut Cx, room_id: OwnedRoomId, event_id: OwnedEventId) -> Result<(), String> {
    queue_room_action(cx, room_id, RoomAction::JumpToEvent(event_id))
}

/// Archives the working copy as a new version and points the app at it.
fn commit_version(
    manifest: &mut MiniAppManifest, origin: VersionOrigin, note: &str,
    actor: Option<VersionActor>, acquired_from: Option<AcquisitionSource>,
) -> Result<(), String> {
    let parent = manifest.current_version.clone();
    let mut version = versions::new_version(
        manifest, origin, note, parent.as_deref(), versions::now_unix(), utc_offset_secs(),
    );
    version.actor = actor;
    version.host_version = Some(env!("CARGO_PKG_VERSION").into());
    version.host_revision = Some(env!("ROBRIX_GIT_COMMIT_HASH")).filter(|revision| !revision.is_empty()).map(str::to_string);
    version.acquired_from = acquired_from.or(version.acquired_from);
    manifest.current_version = Some(persistence::append_version(manifest, version)
        .map_err(|error| format!("Could not save this mini-app's history: {error}"))?);
    Ok(())
}

/// Before the working copy gets replaced: makes sure it is a version, so it
/// stays reachable. A pristine built-in archives as its stock version.
fn archive_current(manifest: &mut MiniAppManifest) -> Result<(), String> {
    let pristine = manifest.builtin
        && builtin::stock(&manifest.id).is_some_and(|stock| builtin::matches_default(manifest, &stock));
    let (origin, note) = if pristine {
        (VersionOrigin::Stock, "Stock")
    } else {
        (VersionOrigin::Legacy, "Before version history")
    };
    persistence::ensure_current_version(manifest, origin, note, 0, 0)
        .map_err(|error| format!("Could not save this mini-app's initial history: {error}"))
}

/// Ensures the current app has an initial history record before showing it.
pub fn ensure_app_history(app_id: &str) {
    with_a2app(|state| {
        if let Some(mut manifest) = state.registry.get(app_id).cloned() {
            if let Err(error) = archive_current(&mut manifest) {
                error!("{error}");
                return;
            }
            state.registry.insert(manifest);
        }
    });
}

/// Restores this build's stock source and records when that change happened.
fn on_stock(mut current: MiniAppManifest, mut stock: MiniAppManifest, actor: Option<VersionActor>) -> Result<MiniAppManifest, String> {
    archive_current(&mut current)?;
    stock.scope = current.scope;
    stock.current_version = current.current_version;
    let source = AcquisitionSource::BuiltIn { app_id: stock.id.clone() };
    let note = if actor.is_some() { "Reset to the built-in version" } else { "Updated built-in version" };
    commit_version(&mut stock, VersionOrigin::Stock, note, actor, Some(source))?;
    Ok(stock)
}

/// Makes `updated` the app's working copy: saved, registered, restarted
/// where it runs, and announced.
fn install_version(cx: &mut Cx, ui: &WidgetRef, updated: MiniAppManifest, done: String) {
    if let Err(error) = persistence::save_user_app(&updated) {
        enqueue_popup_notification(format!("Could not save this mini-app: {error}"), PopupKind::Error, Some(5.0));
        return;
    }
    with_a2app(|state| {
        state.clear_adopted_builtin_update(&updated);
        state.registry.insert(updated.clone());
    });
    publish_grants(cx);
    let was_running = stop_for_restart(cx, ui, &updated);
    if !super::information_flow::manifest_has_private_source(&updated) {
        // All app instances have stopped. Retire source-inspection contexts
        // too, so an empty stock sandbox does not inherit an edit's activation.
        for snapshot in a2app_core::information_flow::contexts().unwrap_or_default().into_iter()
            .filter(|snapshot| snapshot.context.app() == Some(updated.id.as_str()))
        {
            let _ = a2app_core::information_flow::remove_context_for_activation(&snapshot.context, snapshot.epoch);
        }
        if let Err(error) = super::information_flow::reconcile_builtin_manifest(&updated) {
            enqueue_popup_notification(error, PopupKind::Error, Some(8.0));
        }
    }
    cx.action(A2AppRuntimeAction::VersionsChanged(updated.id.clone()));
    enqueue_popup_notification(reopen_hint(done, was_running), PopupKind::Success, Some(5.0));
    ui.redraw(cx);
}

pub(super) fn matrix_link_action(room: Option<String>, url: &str) -> Result<HostAction, String> {
    let matrix_id = MatrixToUri::parse(url).map(|uri| uri.id().clone())
        .or_else(|_| MatrixUri::parse(url).map(|uri| uri.id().clone()))
        .map_err(|_| String::from("not a matrix.to or matrix: link"))?;
    match matrix_id {
        MatrixId::User(user_id) => Ok(HostAction::ShowUser { room, user_id: user_id.to_string() }),
        MatrixId::Room(room_id) => Ok(HostAction::OpenRoom { room: room_id.to_string() }),
        MatrixId::Event(room_or_alias, event_id) => {
            let room_id = OwnedRoomId::try_from(room_or_alias)
                .map_err(|_| String::from("room aliases can't be resolved yet"))?;
            Ok(HostAction::JumpToEvent { room: Some(room_id.to_string()), event_id: event_id.to_string() })
        }
        _ => Err(String::from("room aliases can't be resolved yet")),
    }
}

pub(super) fn host_action_target_room(action: &HostAction) -> Option<&str> {
    match action {
        HostAction::OpenRoom { room } => Some(room),
        HostAction::OpenSpace { space } => Some(space),
        HostAction::JumpToEvent { room, .. } | HostAction::OpenThread { room, .. }
        | HostAction::ShowUser { room, .. } | HostAction::OpenApp { room, .. }
        | HostAction::ComposerInsert { room, .. } | HostAction::ComposerReplyTo { room, .. }
            => room.as_deref(),
        _ => None,
    }
}

pub(super) fn permission_target_room(service: &str, args: &serde_json::Value, origin: Option<&str>) -> Option<String> {
    if service == "nav.link" {
        let room = args["room_id"].as_str().map(str::trim).filter(|room| !room.is_empty()).or(origin);
        let action = matrix_link_action(room.map(str::to_owned), args["url"].as_str()?).ok()?;
        return host_action_target_room(&action).map(str::to_owned);
    }
    services::permission_context(service, args, origin).target_room.map(str::to_owned)
}

#[cfg(test)]
mod matrix_link_scope_tests {
    use super::*;

    #[test]
    fn matrix_links_use_the_parsed_room_instead_of_the_callers_origin() {
        for url in ["https://matrix.to/#/!target:test", "matrix:roomid/target:test",
            "https://matrix.to/#/!target:test/$event:test"]
        {
            let args = serde_json::json!({ "url": url, "room_id": "!spoofed:test" });
            assert_eq!(permission_target_room("nav.link", &args, Some("!origin:test")), Some("!target:test".into()));
            let action = matrix_link_action(Some("!origin:test".into()), url).unwrap();
            assert_eq!(host_action_target_room(&action), Some("!target:test"));
        }
        let user = serde_json::json!({ "url": "https://matrix.to/#/@person:test" });
        assert_eq!(permission_target_room("nav.link", &user, Some("!origin:test")), Some("!origin:test".into()));
        assert!(matrix_link_action(Some("!origin:test".into()), "https://matrix.to/#/#alias:test").is_err());
    }

    #[test]
    fn a_scoped_link_grant_covers_the_destination_and_keeps_the_session_origin() {
        let mut manifest = builtin::stock("room-peek").unwrap();
        manifest.declare(Permission::RobrixNavigation);
        manifest.capabilities.push("host.nav.link".into());
        let cap = a2app_core::capabilities::by_id("host.nav.link").unwrap();
        let args = serde_json::json!({ "url": "https://matrix.to/#/!target:test" });
        let target = permission_target_room("nav.link", &args, Some("!origin:test")).unwrap();
        let mut store = PermissionStore::default();
        store.grant_scoped(&manifest.id, Permission::RobrixNavigation, Some(cap.id), RoomScope::room("!target:test"),
            GrantDuration::RoomSession, Some("!origin:test")).unwrap();
        let context = PermissionContext { origin_room: Some("!origin:test"), target_room: Some(&target) };
        assert_eq!(store.effective_capability_in_context(&manifest, cap, context), Effective::Granted);
        assert_eq!(store.effective_capability_in_context(&manifest, cap,
            PermissionContext { origin_room: Some("!origin:test"), target_room: Some("!other:test") }), Effective::NeedsPrompt);
        assert_eq!(store.effective_capability_in_context(&manifest, cap,
            PermissionContext { origin_room: Some("!replacement:test"), target_room: Some(&target) }), Effective::NeedsPrompt);
    }

    #[test]
    fn observing_selected_rooms_can_subscribe_from_a_different_origin_and_filters_events() {
        let previous = A2APP.with(|state| state.replace(None));
        let mut manifest = builtin::stock("room-peek").unwrap();
        manifest.permissions = vec![Permission::RobrixObserve.as_str().into()];
        manifest.capabilities = vec!["on_active_room_changed".into()];
        initialize_background_test(manifest.clone());
        let origin = OwnedRoomId::try_from("!origin:test").unwrap();
        with_a2app(|state| {
            state.permissions.grant_scoped(&manifest.id, Permission::RobrixObserve, None, RoomScope::room("!target:test"),
                GrantDuration::RoomSession, Some(origin.as_str())).unwrap();
            let cap = a2app_core::capabilities::by_id("on_active_room_changed").unwrap();
            assert!(services::is_room_collection(cap));
            assert_eq!(state.permissions.effective_collection_capability_in_context(&manifest, cap,
                PermissionContext { origin_room: Some(origin.as_str()), target_room: Some(origin.as_str()) }), Effective::Granted);
        });
        let selected = serde_json::json!({ "room_id": "!target:test", "name": "Selected" });
        let other = serde_json::json!({ "room_id": "!other:test", "name": "Other" });
        assert!(permission_filtered_hook(&manifest.id, Some(&origin), "on_active_room_changed", &selected).is_some());
        assert!(permission_filtered_hook(&manifest.id, Some(&origin), "on_active_room_changed", &other).is_none());
        A2APP.with(|state| { state.replace(previous); });
    }

    #[test]
    fn unread_totals_accept_all_rooms_consent_but_keep_subset_and_room_denials_protected() {
        let previous = A2APP.with(|state| state.replace(None));
        let mut manifest = builtin::stock("room-peek").unwrap();
        manifest.permissions = vec![Permission::MatrixRoomsList.as_str().into()];
        manifest.capabilities = vec!["on_unread_totals_changed".into()];
        initialize_background_test(manifest.clone());
        let origin = OwnedRoomId::try_from("!origin:test").unwrap();
        let payload = serde_json::json!({ "unread": 4, "mentions": 1 });
        with_a2app(|state| {
            state.permissions.grant_scoped(&manifest.id, Permission::MatrixRoomsList, None, RoomScope::room("!target:test"),
                GrantDuration::RobrixSession, None).unwrap();
        });
        assert!(permission_filtered_hook(&manifest.id, Some(&origin), "on_unread_totals_changed", &payload).is_none());
        with_a2app(|state| {
            state.permissions.grant_scoped(&manifest.id, Permission::MatrixRoomsList, None, RoomScope::AllRooms,
                GrantDuration::RobrixSession, None).unwrap();
        });
        assert!(permission_filtered_hook(&manifest.id, Some(&origin), "on_unread_totals_changed", &payload).is_some());
        with_a2app(|state| state.permissions.set_room_policy("!target:test", RoomAccess::Read, PolicyDecision::Deny));
        assert!(permission_filtered_hook(&manifest.id, Some(&origin), "on_unread_totals_changed", &payload).is_none());
        A2APP.with(|state| { state.replace(previous); });
    }
}

fn perform_host_action(cx: &mut Cx, ui: &WidgetRef, heap: usize, action: HostAction) -> Result<(), String> {
    let room_of = |room: Option<String>| -> Result<OwnedRoomId, String> {
        let room = room.ok_or("this mini-app is not attached to a room; pass {room_id}")?;
        OwnedRoomId::try_from(room.as_str()).map_err(|_| String::from("not a valid room id"))
    };
    let event_of = |event_id: &str| {
        OwnedEventId::try_from(event_id).map_err(|_| String::from("not a valid event id"))
    };
    match action {
        HostAction::OpenRoom { room } => {
            let room_id = room_of(Some(room))?;
            let destination_room = BasicRoomDetails::Name(joined_room_name(cx, &room_id)?);
            cx.action(NavigationBarAction::GoToHome);
            cx.action(AppStateAction::NavigateToRoom { room_to_close: None, destination_room });
        }
        HostAction::OpenThread { room, event_id } => {
            let room_id = room_of(room)?;
            let thread_root_event_id = event_of(&event_id)?;
            let room_name_id = joined_room_name(cx, &room_id)?;
            cx.action(NavigationBarAction::GoToHome);
            cx.widget_action(
                ui.widget_uid(),
                RoomsListAction::Selected(SelectedRoom::Thread { room_name_id, thread_root_event_id }),
            );
        }
        HostAction::OpenSpace { space } => {
            let space_id = room_of(Some(space))?;
            let space_name_id = joined_room_name(cx, &space_id)?;
            cx.action(NavigationBarAction::GoToSpace { space_name_id });
        }
        HostAction::OpenScreen { screen } => {
            let action = match screen.as_str() {
                "home" => NavigationBarAction::GoToHome,
                "add_room" => NavigationBarAction::GoToAddRoom { search_for: None },
                "mini_apps" => NavigationBarAction::GoToMiniApps,
                "settings" => NavigationBarAction::OpenSettings,
                _ => return Err(String::from("screen must be one of home, add_room, mini_apps, settings")),
            };
            cx.action(action);
        }
        HostAction::OpenLink { room, url } => {
            let action = matrix_link_action(room, &url)?;
            let context = super::information_flow::context_for_heap(heap)?;
            let capability = a2app_core::capabilities::by_id("host.nav.link").ok_or("Unknown navigation action.")?;
            let allowed = with_a2app(|state| {
                let Some(manifest) = context.app().and_then(|app| state.registry.get(app)) else { return false };
                state.permissions.effective_capability_in_context(manifest, capability, PermissionContext {
                    origin_room: context.room(), target_room: host_action_target_room(&action),
                }) == Effective::Granted
            }).unwrap_or(false);
            if !allowed { return Err("Opening this link is not allowed for its destination room.".into()); }
            return perform_host_action(cx, ui, heap, action);
        }
        HostAction::OpenApp { room, app_id } => {
            let from = super::information_flow::context_for_heap(heap)?;
            let manifest = with_a2app(|state| state.registry.get(&app_id).cloned()).flatten()
                .ok_or("The requested mini-app is not installed.")?;
            let target = room.as_deref().or_else(|| match &manifest.scope {
                A2AppScope::Room { room_id } => Some(room_id.as_str()), A2AppScope::Account => None,
            });
            let room_id = target.map(|target| OwnedRoomId::try_from(target)
                .map_err(|_| String::from("The mini-app's room or space ID is invalid."))).transpose()?;
            let is_space = app_launch_context(&manifest, room_id.as_deref())?;
            let to = super::information_flow::app_context(&manifest.id, target)?;
            a2app_core::information_flow::register_context_with_legacy_data(&to, super::information_flow::manifest_has_private_source(&manifest))?;
            a2app_core::information_flow::transfer(&from, &to)?;
            a2app_core::information_flow::transfer(&to, &from)?;
            let installed = with_a2app(|state| {
                state.registry.get(&app_id).map(|_| state.permissions.is_restricted(&app_id))
            }).flatten();
            match installed {
                None => return Err(String::from("no such app")),
                Some(true) => return Err(String::from("that app is stopped for hammering the host")),
                Some(false) => {}
            }
            a2app_core::information_flow::commit_exact_action_for_activation(&from,
                a2app_core::information_flow::context_epoch(&from)?, &a2app_core::information_flow::SensitiveAction {
                    kind: "host.nav.app".into(), target: app_id.clone(),
                }, &serde_json::json!({ "app_id": app_id, "room_id": target }))?;
            let in_room_pane = room_id.is_some() && !is_space;
            apply_op(cx, ui, A2AppOp::OpenApp { app_id, room_id, in_room_pane });
        }
        HostAction::JumpToEvent { room, event_id } => {
            let room_id = room_of(room)?;
            queue_room_action(cx, room_id, RoomAction::JumpToEvent(event_of(&event_id)?))?;
        }
        HostAction::ShowUser { room, user_id } => {
            let room_id = room_of(room)?;
            let user_id = OwnedUserId::try_from(user_id.as_str())
                .map_err(|_| String::from("not a valid user id"))?;
            queue_room_action(cx, room_id, RoomAction::ShowUserProfile(user_id))?;
        }
        HostAction::ComposerInsert { room, text } => {
            let room_id = room_of(room)?;
            queue_composer_action(cx, heap, room_id, RoomAction::InsertDraft(text), "host.composer.insert")?;
        }
        HostAction::ComposerReplyTo { room, event_id } => {
            let room_id = room_of(room)?;
            queue_composer_action(cx, heap, room_id, RoomAction::ReplyTo(event_of(&event_id)?), "host.composer.reply_to")?;
        }
        HostAction::ClosePane | HostAction::SetSide { .. } | HostAction::BreakOut
        | HostAction::Minimize | HostAction::Restore => {
            let key = instances::key_of_heap(heap).ok_or("this instance has no pane")?;
            let op = match (&action, instances::surface_of(&key)) {
                // Already minimized, or its room isn't shown: it stays minimized when that room is shown again.
                (HostAction::Minimize, None) if key.1.is_some() => {
                    instances::request_pane(&key, instances::PaneRequest::Minimize);
                    return Ok(());
                }
                (HostAction::Minimize, Some(Surface::Dock)) => {
                    instances::note_minimizing(&key);
                    RoomPaneOp::Minimize
                }
                // Already on screen, unless it's about to be minimized.
                (HostAction::Restore, Some(_)) => {
                    instances::request_pane(&key, instances::PaneRequest::Restore);
                    return Ok(());
                }
                (HostAction::Restore, None) => {
                    let is_joined_room = key.1.as_ref().is_some_and(|room_id| crate::sliding_sync::get_client()
                        .and_then(|client| client.get_room(room_id))
                        .is_some_and(|room| !room.is_space() && room.state() == RoomState::Joined));
                    if !is_joined_room {
                        return Err(String::from("only an app in a joined room can restore its pane"));
                    }
                    // The room's screen docks it once the user is looking at that room.
                    if !instances::request_pane(&key, instances::PaneRequest::Restore) {
                        return Err(String::from("the user closed this app"));
                    }
                    return Ok(());
                }
                // The caller closes the modal once this answers.
                (HostAction::ClosePane, Some(Surface::Modal)) => return Ok(()),
                (HostAction::ClosePane, None) => {
                    if instances::quit(cx, &key) {
                        cx.action(MiniAppInstanceAction::AppStopped(key.0));
                    }
                    return Ok(());
                }
                (HostAction::ClosePane, _) => RoomPaneOp::Close,
                (HostAction::BreakOut, Some(Surface::Tab)) => return Ok(()),
                (HostAction::SetSide { side }, Some(Surface::Dock)) => {
                    RoomPaneOp::MoveTo(super::room_panes::from_app_side(*side))
                }
                (HostAction::BreakOut, Some(Surface::Dock)) => RoomPaneOp::PopOut,
                _ => return Err(String::from("this instance is not docked in a room")),
            };
            let (app_id, room_id) = key;
            let room_id = room_id.ok_or("this instance is not docked in a room")?;
            room_pane::request(cx, room_id, RoomPaneKind::MiniApp(app_id), op);
        }
    }
    Ok(())
}

/// The display settings an app may read, also the `on_prefs_changed` payload.
fn prefs_json(cx: &mut Cx) -> serde_json::Value {
    let desktop = effective_is_desktop(cx);
    let prefs = &cx.global::<AppPreferencesGlobal>().0;
    serde_json::json!({
        "view_mode": if desktop { "desktop" } else { "mobile" },
        "view_mode_override": match prefs.view_mode {
            ViewModeOverride::Automatic => "auto",
            ViewModeOverride::ForceWide => "desktop",
            ViewModeOverride::ForceNarrow => "mobile",
        },
        "ui_zoom": prefs.ui_zoom.0,
        "send_on_enter": prefs.send_on_enter,
        "thumbnail_max_height": match prefs.thumbnail_max_height {
            ThumbnailMaxHeight::Small => 200,
            ThumbnailMaxHeight::Medium => 300,
            ThumbnailMaxHeight::Large => 400,
            ThumbnailMaxHeight::Custom(px) => px,
        },
        "show_read_receipts": prefs.show_read_receipts,
    })
}

fn queue_permission_prompt(
    cx: &mut Cx,
    ui: &WidgetRef,
    subject: String,
    perm: Permission,
    parked: ParkedRequest,
    tool: Option<PromptToolGrant>,
) {
    // "Not Now" this session: refuse without re-asking, so a looping caller
    // (a script, or an agent that keeps trying the same tool) can't nag its
    // way to an accidental Allow. Refusing re-enters the state, so it runs
    // outside the borrow below.
    permission_batch::retry_parked(&subject, perm, &parked);
    let dismissed = with_a2app(|state| state.dismissed_prompts.contains(&(subject.clone(), perm)))
        .unwrap_or(false);
    if dismissed {
        refuse_parked_request(cx, perm, parked);
        return;
    }
    if tool.is_none() && let ParkedRequest::Bridge(Some(request)) = &parked
        && let Ok(Some(review)) = combined_bridge_review(request)
    {
        let enable_writes = with_a2app(|state| parked_can_enable_writes(state, &subject, perm, &parked)).unwrap_or(false);
        if !enable_writes || review.allow_session() {
            let ParkedRequest::Bridge(Some(request)) = parked else { unreachable!() };
            queue_flow_prompt(cx, ui, FlowContinuation::Bridge { request, review, allow_once: !enable_writes, permission: Some(perm) });
            return;
        }
        // Enable the master switch through its ordinary popup first when the
        // actual effect can only receive exact one-time approval. Otherwise
        // neither duration would be available in the combined popup.
        let _ = a2app_core::information_flow::cancel_effect(review.id);
    }
    with_a2app(|state| {
        let activation = match &parked {
            ParkedRequest::Bridge(Some(request)) => super::information_flow::context_for_heap(request.heap_key).ok()
                .and_then(|context| a2app_core::information_flow::context_epoch(&context).ok()
                    .map(|epoch| (request.heap_key, request.req_id, context, epoch))),
            _ => None,
        };
        let enable_writes = parked_can_enable_writes(state, &subject, perm, &parked);
        state.prompts.push_back(PermissionPrompt {
            setup: None, id: next_permission_prompt_id(),
            subject,
            perm,
            parked: vec![parked],
            tool,
            flow: None,
            enable_writes,
            activations: activation.into_iter().collect(),
        });
    });
    show_next_permission_prompt(cx, ui);
}

/// Combine ordinary access and an immediate outgoing effect into one question.
///
/// Deferred Matrix effects still resolve their actual SDK operation at the
/// worker boundary. Local reads have no outgoing data-sharing requirement.
fn combined_bridge_review(request: &SplashHostRequest) -> Result<Option<a2app_core::information_flow::EffectReview>, String> {
    let context = super::information_flow::context_for_heap(request.heap_key)?;
    let args: serde_json::Value = serde_json::from_str(&request.args_json).map_err(|error| error.to_string())?;
    if request.service == "network.http" {
        let review = super::network::Request::parse(&args)?.permission_review(&context)?;
        return Ok((!review.allowed).then_some(review));
    }
    if request.service.starts_with("matrix.") { return matrix_permission_review(request, &context, &args); }
    if matches!(request.service.as_str(),
        "host.composer.insert" | "host.composer.reply_to" | "host.nav.app") { return Ok(None); }
    let Some(capability) = a2app_core::capabilities::for_service(&request.service) else { return Ok(None) };
    let contract = capability.flow_contract().ok_or("Missing data-flow contract.")?;
    let target_room = permission_target_room(&request.service, &args, context.room());
    let target = target_room.as_deref();
    let homeserver = crate::sliding_sync::get_client().map(|client| client.homeserver().to_string());
    let recipient = contract.recipient(context.account(), target, &args, homeserver.as_deref())?;
    let action = contract.sensitive_action(capability.id, &args, target);
    if recipient.is_none() && action.is_none() { return Ok(None); }
    let epoch = a2app_core::information_flow::context_epoch(&context)?;
    let review = a2app_core::information_flow::prepare_effect_for_activation(&context, epoch, recipient.as_ref(), action.as_ref(), &args)?;
    Ok((!review.allowed).then_some(review))
}

/// Capture local SDK metadata before asking about a remote Matrix operation.
///
/// Preflight changes only a private policy clone and provenance labels. It
/// performs no network work, grants no access, and reads no message contents.
fn matrix_permission_review(request: &SplashHostRequest, context: &a2app_core::information_flow::ContextId, args: &serde_json::Value) -> Result<Option<a2app_core::information_flow::EffectReview>, String> {
    use a2app_core::{capabilities::{FlowOutput, FlowSource}, information_flow::{self as flow, Influence, Recipient, SensitiveAction}};
    let capability = a2app_core::capabilities::for_service(&request.service).ok_or("Unknown Matrix action.")?;
    let contract = capability.flow_contract().ok_or("Missing data-flow contract.")?;
    let call = services::matrix::parse(&request.service, args, context.room().is_some())?;
    let permission_context = services::permission_context(&request.service, args, context.room());
    let Some(client) = crate::sliding_sync::get_client() else { return Ok(None) };
    let epoch = flow::context_epoch(context)?;
    let mut searched_sources = flow::Label::new();
    let sensitive = |target: &str, payload: serde_json::Value| -> Result<(Recipient, Option<SensitiveAction>, serde_json::Value), String> {
        let recipient = contract.recipient(context.account(), Some(target), &payload, Some(client.homeserver().as_str()))?
            .ok_or("Missing Matrix action destination.")?;
        Ok((recipient, Some(SensitiveAction { kind: capability.id.into(), target: target.into() }), payload))
    };
    let (recipient, action, payload) = match call {
        services::MatrixServiceCall::SendMessage { body }
        | services::MatrixServiceCall::RoomsSend { body, .. } => {
            let room = permission_context.target_room.ok_or("Missing message destination.")?;
            let content = matrix_sdk::ruma::events::room::message::RoomMessageEventContent::text_plain(body);
            sensitive(room, serde_json::to_value(content).map_err(|_| "Cannot review message content.")?)?
        }
        services::MatrixServiceCall::Pin { event_id, pinned } => {
            let room = permission_context.target_room.ok_or("Missing pin destination.")?;
            let event_id = matrix_sdk::ruma::OwnedEventId::try_from(event_id).map_err(|_| "invalid event_id")?;
            sensitive(room, serde_json::json!({ "event_id": event_id, "pinned": pinned }))?
        }
        services::MatrixServiceCall::RoomFlag { flag, on } => {
            let room = permission_context.target_room.ok_or("Missing room flag destination.")?;
            let flag_name = match flag {
                services::matrix::RoomFlag::Favorite => "favorite",
                services::matrix::RoomFlag::LowPriority => "low_priority",
                services::matrix::RoomFlag::Unread => "unread",
            };
            sensitive(room, serde_json::json!({ "flag": flag_name, "on": on }))?
        }
        services::MatrixServiceCall::InviteRespond { room_id, accept } => {
            let room = matrix_sdk::ruma::OwnedRoomId::try_from(room_id).map_err(|_| "invalid room_id")?;
            sensitive(room.as_str(), serde_json::json!({ "accept": accept }))?
        }
        services::MatrixServiceCall::Invite { user_id } => {
            let room = permission_context.target_room.ok_or("Missing invite destination.")?;
            let user_id = matrix_sdk::ruma::OwnedUserId::try_from(user_id).map_err(|_| "invalid user_id")?;
            sensitive(room, serde_json::json!({ "user_id": user_id }))?
        }
        services::MatrixServiceCall::Typing { typing } => {
            if typing && !crate::settings::app_preferences::send_typing_notices() { return Ok(None); }
            let room = permission_context.target_room.ok_or("Missing typing destination.")?;
            sensitive(room, serde_json::json!({ "typing": typing }))?
        }
        services::MatrixServiceCall::Join { room, via } => {
            // Aliases must first be resolved by the guarded worker lookup.
            if !room.starts_with('!') { return Ok(None); }
            let room = matrix_sdk::ruma::OwnedRoomId::try_from(room).map_err(|_| "invalid room_id")?;
            let via = via.into_iter().map(matrix_sdk::ruma::OwnedServerName::try_from)
                .collect::<Result<Vec<_>, _>>().map_err(|_| "invalid via server name")?;
            sensitive(room.as_str(), serde_json::json!({ "operation": "join", "room_id": room, "via": via }))?
        }
        services::MatrixServiceCall::Search { query, scope, limit, server: true } => {
            let mut store = with_a2app(|state| state.permissions.clone()).ok_or("Mini Apps are unavailable.")?;
            let subject = context.app().ok_or("Missing mini-app identity.")?;
            store.grant_scoped(subject, capability.group.ok_or("Missing permission group.")?, Some(capability.id),
                bridge_scope(request, capability, &args, permission_context.target_room), GrantDuration::RobrixSession, context.room())?;
            let auth = matrix::policy::MatrixAuthorization::new(subject, capability.id, context.room(), &store).with_flow(context.clone());
            let targets = match scope {
                services::SearchScope::Attached => context.room().and_then(|id| matrix_sdk::ruma::RoomId::parse(id).ok())
                    .and_then(|id| client.get_room(&id)).into_iter().collect::<Vec<_>>(),
                services::SearchScope::AllJoined => client.joined_rooms().into_iter().filter(|room| !room.is_space()).collect(),
                services::SearchScope::Rooms(ids) => ids.iter().filter_map(|id| matrix_sdk::ruma::RoomId::parse(id).ok())
                    .filter_map(|id| client.get_room(&id)).filter(|room| room.state() == matrix_sdk::RoomState::Joined && !room.is_space()).collect(),
            };
            let readable = targets.iter().filter(|room|
                store.room_policy(Some(room.room_id().as_str()), RoomAccess::Read) != PolicyDecision::Deny
                    && auth.permits(&store, Some(room.room_id().as_str()))).collect::<Vec<_>>();
            let unencrypted = readable.iter().filter(|room| !room.encryption_state().is_encrypted())
                .map(|room| room.room_id().to_owned()).collect::<Vec<_>>();
            if unencrypted.is_empty() { return Ok(None); }
            searched_sources.extend(readable.iter().map(|room| flow::Source::Room {
                account: context.account().into(), room: room.room_id().to_string(),
            }));
            let parameters = matrix::policy::search_server_parameters(&query, &unencrypted, limit);
            (Recipient::network_origin(client.homeserver().as_str())?, None,
                serde_json::json!({ "destination": client.homeserver().as_str(), "operation": capability.id, "parameters": parameters }))
        }
        _ if !contract.privileged_effect && matches!(contract.output, FlowOutput::MatrixServer | FlowOutput::MatrixPagination) => {
            let Some(recipient) = contract.recipient(context.account(), permission_context.target_room, args, Some(client.homeserver().as_str()))? else { return Ok(None) };
            (recipient, None, serde_json::json!({ "destination": client.homeserver().as_str(), "operation": capability.id, "parameters": args }))
        }
        _ => return Ok(None),
    };
    // Record the declared incoming source now so final dispatch cannot add
    // an already-disclosed source and force a second permission popup.
    if !matches!(contract.source, FlowSource::InstalledAppCode | FlowSource::IpcAppCode | FlowSource::Peer) {
        let sources = contract.source_labels(context.account(), context.room(), permission_context.target_room)?;
        flow::add_sources_for_activation(context, epoch, sources.clone())?;
        if contract.untrusted_content {
            flow::add_influences_for_activation(context, epoch, sources.into_iter().map(|source| match source {
                flow::Source::Room { account, room } => Influence::RoomContent { account, room },
                _ => Influence::Unknown,
            }))?;
        }
    }
    flow::add_sources_for_activation(context, epoch, searched_sources.clone())?;
    flow::add_influences_for_activation(context, epoch, searched_sources.into_iter().filter_map(|source| match source {
        flow::Source::Room { account, room } => Some(Influence::RoomContent { account, room }),
        _ => None,
    }))?;
    let review = flow::prepare_effect_for_activation(context, epoch, Some(&recipient), action.as_ref(), &payload)?;
    Ok((!review.allowed).then_some(review))
}

fn queue_flow_prompt(cx: &mut Cx, ui: &WidgetRef, flow: FlowContinuation) {
    let review = flow.review();
    if permission_batch::effect_dismissed(review) {
        refuse_flow(cx, flow, "This request was not approved.");
        return;
    }
    if !flow.can_prompt() {
        refuse_flow(cx, flow, "Open this mini-app to review the permission it needs.");
        return;
    }
    let subject = review.context.app().map(str::to_owned)
        .unwrap_or_else(|| review.context.room().map(agent_subject).unwrap_or_default());
    let perm = match &flow { FlowContinuation::Bridge { permission: Some(permission), .. } => Some(*permission), _ => None }
        .or_else(|| review.action.as_ref().and_then(|action| a2app_core::capabilities::by_id(&action.kind))
            .and_then(|capability| capability.group)).unwrap_or(Permission::Network);
    with_a2app(|state| state.prompts.push_back(PermissionPrompt {
        setup: None, id: next_permission_prompt_id(), subject, perm, parked: Vec::new(), tool: None, flow: Some(flow), enable_writes: false, activations: Vec::new(),
    }));
    show_next_permission_prompt(cx, ui);
}

fn refuse_flow(cx: &mut Cx, flow: FlowContinuation, reason: &str) {
    match flow {
        FlowContinuation::Worker(worker) => worker.finish(Err(reason.into())),
        FlowContinuation::Generated { review } => {
            let _ = a2app_core::information_flow::cancel_effect(review.id);
            let cancelled = with_a2app(|state| {
                if !state.pending_generated.as_ref().is_some_and(|pending| pending.matches_review(&review))
                { return false; }
                state.pending_generated = None;
                state.generation = None;
                state.generation_context = None; state.generation_epoch = None;
                state.failed_request = None;
                state.console.status = "The app was not installed. You can start a new request.".into();
                true
            }).unwrap_or(false);
            #[cfg(unix)]
            if cancelled { resolve_session_generation(cx, &WidgetRef::empty(), None, Err(format!("The app was not installed: {reason}"))); }
            #[cfg(not(unix))]
            let _ = cancelled;
        }
        FlowContinuation::Bridge { request, review, .. } => {
            let _ = a2app_core::information_flow::cancel_effect(review.id);
            if a2app_core::information_flow::ensure_context_epoch(&review.context, review.epoch).is_ok()
                && super::information_flow::context_for_heap(request.heap_key).as_ref() == Ok(&review.context)
            {
                with_a2app(|state| state.broker.declined(&request));
                services::respond(cx, Reply { heap_key: request.heap_key, req_id: request.req_id }, Err(reason));
            }
        }
    }
}

fn replay_bridge_request(cx: &mut Cx, ui: &WidgetRef, request: SplashHostRequest) {
    let asks = with_a2app(|state| {
        let A2AppState { broker, registry, permissions, foreground_app, .. } = state;
        let is_docked = |app_id: &str| instances::is_foreground(app_id);
        let desktop_view = effective_is_desktop(cx);
        let rooms = cx.has_global::<RoomsListRef>().then(|| cx.get_global::<RoomsListRef>().clone());
        let room_name = |id: &str| room_display_name(rooms.as_ref()?, id);
        broker.dispatch_after_grant(cx, BrokerCtx {
            registry, permissions, foreground_app: foreground_app.as_deref(),
            is_docked: &is_docked, is_running: &instances::is_running,
            pane_state: &instances::pane_state, storage_path: &compartment_storage_path,
            room_name: &room_name, desktop_view,
            permission_target_room: Some(&permission_target_room),
            check_flow: &super::information_flow::check_request,
            check_response: &super::information_flow::check_response,
        }, request)
    }).unwrap_or_default();
    for ask in asks { apply_broker_ask(cx, ui, ask); }
}

fn bridge_activation_is_live(request: &SplashHostRequest, activations: &[(usize, u64, a2app_core::information_flow::ContextId, u64)]) -> bool {
    activations.iter().find(|(heap, id, _, _)| *heap == request.heap_key && *id == request.req_id)
        .is_some_and(|(_, _, context, epoch)| a2app_core::information_flow::ensure_context_epoch(context, *epoch).is_ok()
            && super::information_flow::context_for_heap(request.heap_key).as_ref() == Ok(context))
}

fn bridge_activation_can_prompt(request: &SplashHostRequest, activations: &[(usize, u64, a2app_core::information_flow::ContextId, u64)]) -> bool {
    bridge_activation_is_live(request, activations) && activations.iter()
        .find(|(heap, id, _, _)| *heap == request.heap_key && *id == request.req_id)
        .is_some_and(|(_, _, context, epoch)| instances::context_can_prompt(context, *epoch))
}

fn retain_live_bridge_requests(prompt: &mut PermissionPrompt) {
    let activations = &prompt.activations;
    prompt.parked.retain(|parked| match parked {
        ParkedRequest::Bridge(Some(request)) => bridge_activation_can_prompt(request, activations),
        _ => true,
    });
}

fn sweep_permission_prompts(cx: &mut Cx, ui: &WidgetRef) {
    permission_batch::sweep(cx, ui);
}

/// The tool name an AI session job maps to (the name the model called).
#[cfg(unix)]
fn ai_job_tool_name(job: &SessionJob) -> String {
    match job {
        SessionJob::ReadTool { kind, .. } => read_tool_name(kind).to_string(),
        SessionJob::RequestTaskPermissions { .. } => String::from("request_task_permissions"),
        SessionJob::LaunchSplashApp { .. } => String::from("launch_splash_app"),
        SessionJob::ListApps { .. } => String::from("list_apps"),
        SessionJob::LaunchApp { .. } => String::from("launch_app"),
        SessionJob::SendRoomMessage { .. } => String::from("send_message"),
        SessionJob::PostRoomMessage { .. } => String::from("post_room_message"),
        // Not a tool call: the agent's web tool is already shown by its ACP
        // events; this job only asks whether a host may be reached.
        SessionJob::NetworkAccess { .. } => String::from("network_access"),
        SessionJob::FetchUrl { .. } => String::from("web_fetch"),
        // A tool a mini-app registered; its name is already namespaced.
        SessionJob::InvokeMiniAppTool { tool, .. } => tool.clone(),
        SessionJob::CallMiniAppTool { .. } => String::from("call_mini_app_tool"),
        SessionJob::ListMiniAppTools { .. } => String::from("list_mini_app_tools"),
    }
}

/// The human-readable target detail for one tool call, rendered after the
/// humanized action in the live row and on the turn's receipt chip. The room
/// and space names come from the rooms list; a missing list (before Home is
/// up) falls back to the raw id. `None` when the call has no target to name.
#[cfg(unix)]
fn session_job_detail(rooms: Option<&RoomsListRef>, job: &SessionJob) -> Option<String> {
    match job {
        SessionJob::ReadTool { kind, .. } => read_kind_detail(kind, &|id| resolve_room_label(rooms, id)),
        SessionJob::PostRoomMessage { room_id, .. } => {
            Some(format!("into “{}”", resolve_room_label(rooms, room_id)))
        }
        SessionJob::LaunchSplashApp { description, .. } => {
            let description = description.trim();
            if description.is_empty() {
                None
            } else {
                let mut clipped: String = description.chars().take(60).collect();
                if description.chars().count() > 60 {
                    clipped.push('…');
                }
                Some(format!("“{clipped}”"))
            }
        }
        // Named so the live row opens (and finishes) with a target even
        // though the reply itself is the visible output; a synchronous-finish
        // tool must not leave a stranded `Started` row when the job reaches
        // the UI thread before the agent's `started` event.
        SessionJob::SendRoomMessage { .. } => Some(String::from("in this room")),
        // A launch names the app it runs; the read-only list has no target.
        SessionJob::LaunchApp { app_id, .. } => Some(format!("“{}”", resolve_app_label(app_id))),
        SessionJob::ListApps { .. } => None,
        // The web tool's own name is the action; the URL is the argument the
        // reader needs to tell one fetch from another.
        SessionJob::NetworkAccess { url, .. } | SessionJob::FetchUrl { url, .. } => {
            (!url.trim().is_empty()).then(|| url.clone())
        }
        // The tool name already names it; no extra target to add.
        SessionJob::InvokeMiniAppTool { .. } => None,
        // A bridged call still targets one app tool; name it for the card.
        SessionJob::CallMiniAppTool { tool, .. } => with_a2app(|state| {
            state.app_tools.get(tool).map(|reg| format!("“{}”", reg.raw_name))
        })
        .flatten(),
        // A listing has no single target of its own.
        SessionJob::ListMiniAppTools { .. } => None,
        // The task prompt names its own task in the modal title.
        SessionJob::RequestTaskPermissions { .. } => None,
    }
}

/// The display name for a room or space id, for the tool cards and prompts:
/// the rooms list's name when it has one, else the SDK's cached name (which
/// also covers spaces and rooms the list has not built yet), else the raw id
/// as a last resort. Never returns an empty string.
#[cfg(unix)]
fn resolve_room_label(rooms: Option<&RoomsListRef>, room_id: &str) -> String {
    if let Some(name) = rooms.and_then(|r| room_display_name(r, room_id)) {
        return name;
    }
    if let Ok(id) = OwnedRoomId::try_from(room_id)
        && let Some(room) = crate::sliding_sync::get_client().and_then(|c| c.get_room(&id))
        && let Some(name) = room.cached_display_name()
    {
        return name.to_string();
    }
    room_id.to_string()
}

/// The display name for an installed app, for tool-card targets: the
/// manifest's name when the app still exists, else the raw id as a last
/// resort. Never returns an empty string (an app id is never empty).
#[cfg(unix)]
fn resolve_app_label(app_id: &str) -> String {
    with_a2app(|state| state.registry.get(app_id).map(|m| m.name.clone()))
        .flatten()
        .unwrap_or_else(|| app_id.to_string())
}

/// The target detail for one read, by kind. Reads confined to this room name
/// no room (the row is already in it); a cross-room or space read names its
/// target so the user can tell where the AI looked.
#[cfg(unix)]
fn read_kind_detail(kind: &ReadToolKind, room_label: &impl Fn(&str) -> String) -> Option<String> {
    match kind {
        ReadToolKind::OtherRoom { room, .. } => Some(format!("in “{}”", room_label(room))),
        ReadToolKind::SpaceInfo { space } => Some(format!("for “{}”", room_label(space))),
        ReadToolKind::SpaceRooms { space } => Some(format!("in “{}”", room_label(space))),
        // This room's own reads and the user-wide lists carry no target.
        ReadToolKind::Messages { .. }
        | ReadToolKind::Older { .. }
        | ReadToolKind::Info
        | ReadToolKind::ListRooms
        | ReadToolKind::ListSpaces
        | ReadToolKind::Memory { .. } => None,
    }
}

/// Refuses one parked request outright (a "Not Now" dismissal): a mini-app
/// bridge request is declined through the broker exactly as a stored Deny
/// would be; an AI tool call is answered with an error the model can read and
/// act on.
fn refuse_parked_request(cx: &mut Cx, perm: Permission, parked: ParkedRequest) {
    match parked {
        ParkedRequest::Bridge(request) => {
            if let Some(request) = request {
                with_a2app(|state| state.broker.declined(&request));
                Broker::respond_denied(cx, &request);
            }
        }
        #[cfg(unix)]
        ParkedRequest::AiTool { room_id, job } => {
            // The prompt was dismissed, not answered: the tool call is refused
            // (with a receipt, so the user sees the AI was stopped from it —
            // and its live tool row rewritten Done).
            let reason = ai_tool_refused_text(perm);
            // Teardown also withdraws prompts, but their receipts must not
            // carry over into a replacement session's first reply.
            if with_a2app(|state| state.ai_sessions.contains_key(&room_id)).unwrap_or(false) {
                note_ai_tool_call(&room_id, &ai_job_tool_name(&job), false, &reason);
            }
            answer_session_job(job, Err(reason));
        }
        #[cfg(unix)]
        ParkedRequest::NetworkAccess { room_id, host, answer, .. } => {
            // Dismissed, not answered: the web tool is refused for this host
            // (the model is told, and can tell the user what it wanted). The
            // turn card's live line is still closed by the agent's own ACP
            // `tool_call_update`, so no extra note is needed here.
            let _ = room_id;
            let _ = answer.send(Err(format!(
                "The user did not allow the AI in this room to reach `{host}`. \
                 Tell them what you wanted to fetch and why, so they can allow it."
            )));
        }
    }
}

/// Withdraws every prompt parked for a room's agent (its turn is over or its
/// session gone): the parked calls are refused and the modal moves on.
#[cfg(unix)]
fn refuse_room_prompts(cx: &mut Cx, ui: &WidgetRef, room_id: &OwnedRoomId) {
    cancel_subject_permission_prompts(cx, ui, &agent_subject(room_id.as_str()));
    show_next_permission_prompt(cx, ui);
}

fn permission_modal_is_open(state: &A2AppState) -> bool {
    if state.permission_batch_busy || state.active_prompt.is_some() {
        return true;
    }
    #[cfg(unix)]
    if state.active_task.is_some() || state.active_exact_review.is_some() {
        return true;
    }
    false
}

fn show_next_permission_prompt(cx: &mut Cx, ui: &WidgetRef) {
    if with_a2app(|state| permission_modal_is_open(state)).unwrap_or(true) {
        return;
    }
    // Exact-action reviews and task prompts show ahead of single-permission
    // prompts, one modal at a time; reviews come first.
    #[cfg(unix)]
    if with_a2app(|state| state.active_exact_review.is_none() && !state.exact_reviews.is_empty()).unwrap_or(false) {
        show_next_exact_review(cx, ui);
        return;
    }
    // Task prompts show ahead of single-permission prompts, one modal at a time.
    #[cfg(unix)]
    if with_a2app(|state| state.active_task.is_none() && !state.task_prompts.is_empty()).unwrap_or(false) {
        show_next_task_prompt(cx, ui);
        return;
    }
    permission_batch::show_next_ordinary(cx, ui);
}

fn cancel_subject_permission_prompts(cx: &mut Cx, ui: &WidgetRef, subject: &str) {
    permission_batch::cancel_subject(cx, ui, subject);
}

/// Only the native host can record a user gesture; a guest retry flag cannot.
pub(super) fn note_permission_gesture(context: &a2app_core::information_flow::ContextId) {
    if let Ok(epoch) = a2app_core::information_flow::context_epoch(context) {
        with_a2app(|state| {
            if let Some(subject) = context.app() { state.permissions.clear_tool_denials_for(subject); }
            state.permission_gestures.insert(context.clone(), (epoch, Instant::now()));
        });
    }
}

/// A grant from the preceding popup can also cover an already queued request.
fn flow_bridge_permission_granted(flow: &FlowContinuation) -> bool {
    let FlowContinuation::Bridge { request, review, permission: Some(_), .. } = flow else { return true };
    let Ok(args) = serde_json::from_str::<serde_json::Value>(&request.args_json) else { return false };
    let Some(capability) = a2app_core::capabilities::for_service(&request.service) else { return false };
    let Some(subject) = review.context.app() else { return false };
    let target = permission_target_room(&request.service, &args, review.context.room());
    let context = PermissionContext { origin_room: review.context.room(), target_room: target.as_deref() };
    with_a2app(|state| {
        let Some(manifest) = state.registry.get(subject) else { return false };
        let effective = if services::is_room_collection(capability) {
            state.permissions.effective_collection_capability_in_context(manifest, capability, context)
        } else { state.permissions.effective_capability_in_context(manifest, capability, context) };
        effective == Effective::Granted && (request.service != "network.http" || args["url"].as_str()
            .is_some_and(|url| state.permissions.is_url_allowed(subject, url, context)))
    }).unwrap_or(false)
}

fn prompt_already_granted(state: &A2AppState, prompt: &PermissionPrompt) -> bool {
    prompt.parked.iter().all(|parked| {
        let (origin, target) = parked_rooms(parked);
        let context = PermissionContext { origin_room: origin.as_deref(), target_room: target.as_deref() };
        if let Some(url) = parked_network_url(parked) { return state.permissions.is_url_allowed(&prompt.subject, &url, context); }
        if let Some(tool) = &prompt.tool {
            return state.permissions.has_scoped_tool_grant(&prompt.subject, &tool.full_name, &tool.content_hash, context);
        }
        let Some(manifest) = state.registry.get(&prompt.subject) else { return false };
        if let Some(capability) = parked_capability(parked) {
            if services::is_room_collection(capability) {
                state.permissions.effective_collection_capability_in_context(manifest, capability, context) == Effective::Granted
            } else { state.permissions.effective_capability_in_context(manifest, capability, context) == Effective::Granted }
        } else { state.permissions.effective_for_in_context(&prompt.subject, |permission| manifest.declares(permission), prompt.perm, context) == Effective::Granted }
    })
}

fn flow_prompt_info(state: &A2AppState, rooms: Option<&RoomsListRef>, flow: &FlowContinuation) -> FlowPromptInfo {
    use a2app_core::information_flow::{Recipient, Source};
    let review = flow.review();
    let manifest = review.context.app().and_then(|app| state.registry.get(app));
    let room_name = |room: &str| rooms.and_then(|rooms| room_display_name(rooms, room)).unwrap_or_else(|| room.into());
    let destination = match review.recipient.as_ref() {
        Some(Recipient::MatrixRoom { room, .. }) => format!("Room: {}", room_name(room)),
        Some(Recipient::NetworkOrigin(origin)) => {
            let is_server = crate::sliding_sync::get_client().is_some_and(|client|
                Recipient::network_origin(client.homeserver().as_str()).as_ref() == Ok(&Recipient::NetworkOrigin(origin.clone())));
            format!("{}: {origin}", if is_server { "Your Matrix server" } else { "Website" })
        }
        Some(Recipient::ModelProvider(_)) => "Your AI service".into(),
        Some(Recipient::Clipboard) => "System clipboard (other apps can read copied text)".into(),
        Some(Recipient::External) => "External destination".into(),
        None => "Robrix".into(),
    };
    let capability = review.action.as_ref().and_then(|action| a2app_core::capabilities::by_id(&action.kind))
        .or_else(|| serde_json::from_str::<serde_json::Value>(&review.payload).ok()
            .and_then(|payload| payload["operation"].as_str().and_then(a2app_core::capabilities::by_id)));
    let payload = serde_json::from_str::<serde_json::Value>(&review.payload).unwrap_or_default();
    let (scope, scope_targets) = flow_prompt_scope(state, flow, capability, &payload);
    let mut action = capability.map(|capability| permission_action(capability,
        payload.get("parameters").unwrap_or(&payload))).unwrap_or_else(|| "Send this request".into());
    if capability.is_some_and(|capability| matches!(capability.id,
        "matrix.room.event.read" | "matrix.room.thread.read" | "matrix.room.messages.paginate"
            | "host.nav.event" | "host.nav.thread"))
    {
        let parameters = payload.get("parameters").unwrap_or(&payload);
        if let Some(room) = parameters["room_id"].as_str().or_else(|| parameters["room"].as_str())
            .or_else(|| review.context.room())
        { action.push_str(&format!("\nIn {}", room_name(room))); }
    }
    if capability.is_some_and(|capability| matches!(capability.id, "matrix.room.messages.search" | "matrix.rooms.messages.search")) {
        if let Some(ids) = payload["parameters"]["room_ids"].as_array() {
            let names = ids.iter().filter_map(serde_json::Value::as_str).map(room_name).collect::<Vec<_>>();
            let shown = names.iter().take(6).cloned().collect::<Vec<_>>().join(", ");
            if !shown.is_empty() {
                action.push_str(&format!("\nRooms: {shown}"));
                if names.len() > 6 { action.push_str(&format!(" and {} more", names.len() - 6)); }
            }
        }
        action.push_str("\nThe search text is sent to your Matrix server. Encrypted rooms are searched on this device.");
    }
    if capability.is_some_and(|capability| capability.id == "matrix.space.rooms.list")
        && let Some(space) = payload["parameters"]["space_id"].as_str()
    { action.push_str(&format!("\nSpace: {}", room_name(space))); }
    if let FlowContinuation::Bridge { request, permission: Some(permission), .. } = flow {
        let manifest = review.context.app().and_then(|app| state.registry.get(app));
        let args = serde_json::from_str::<serde_json::Value>(&request.args_json).unwrap_or_default();
        if manifest.is_some_and(|manifest| services::can_enable_permission_writes(&state.permissions, manifest, *permission,
            services::permission_context(&request.service, &args, review.context.room())))
        { action.push_str(". Approving also turns on room changes for mini-apps. Each app still needs its own permission."); }
    }
    let (app_name, app_icon) = if let FlowContinuation::Generated { .. } = flow {
        if let Some(pending) = state.pending_generated.as_ref().filter(|pending| pending.matches_review(review)) {
            action = format!("Install {} “{}”", if pending.refine_of.is_some() { "the update to" } else { "the new app" }, pending.manifest.name);
        }
        ("Mini Apps".into(), "✨".into())
    } else {
        (manifest.map(|manifest| manifest.name.clone()).unwrap_or_else(|| "Mini-app".into()),
            manifest.map(|manifest| manifest.icon.clone()).unwrap_or_default())
    };
    FlowPromptInfo {
        prompt_id: 0,
        agent: matches!(review.context, a2app_core::information_flow::ContextId::Agent { .. }),
        app_name,
        app_icon,
        action, destination,
        sources: review.sources.iter().map(|source| match source {
            Source::Account { .. } => "Your account data or text entered in this app".into(),
            Source::Room { room, .. } => format!("Data from {}", room_name(room)),
            Source::RoomDirectory { .. } => "Your room and space directory".into(),
            Source::UnknownPrivate => "Older private data with an unknown source".into(),
        }).collect(),
        payload: serde_json::from_str::<serde_json::Value>(&review.payload)
            .and_then(|value| serde_json::to_string_pretty(&value)).unwrap_or_else(|_| review.payload.to_string()),
        scope, scope_targets,
        room_id: review.context.room().map(str::to_owned),
        allow_once: flow.allow_once(),
        allow_lasting: review.allow_session(),
    }
}

fn flow_prompt_scope(state: &A2AppState, flow: &FlowContinuation,
    capability: Option<&a2app_core::capabilities::Capability>, payload: &serde_json::Value,
) -> (Option<RoomScope>, Vec<(String, Vec<String>)>) {
    use a2app_core::information_flow::Recipient;
    let Some(capability) = capability.filter(|capability| capability_has_room_scope(capability)) else { return (None, Vec::new()) };
    let review = flow.review();
    let parameters = payload.get("parameters").unwrap_or(payload);
    let target = match review.recipient.as_ref() {
        Some(Recipient::MatrixRoom { room, .. }) => Some(room.as_str()),
        _ => parameters["room_id"].as_str().or_else(|| parameters["space_id"].as_str())
            .or_else(|| parameters["room"].as_str())
            .or_else(|| review.action.as_ref().map(|action| action.target.as_str()).filter(|target| RoomId::parse(target).is_ok()))
            .or_else(|| (!services::is_room_collection(capability)).then(|| review.context.room()).flatten()),
    };
    let captured_query = payload.get("parameters").is_some();
    let scope = if let FlowContinuation::Bridge { request, .. } = flow && !captured_query {
        let args = serde_json::from_str::<serde_json::Value>(&request.args_json).unwrap_or_default();
        let target = permission_target_room(&request.service, &args, review.context.room());
        bridge_scope(request, capability, &args, target.as_deref())
    } else { capability_scope(capability, parameters, target) };
    let mut targets = Vec::new();
    if let FlowContinuation::Bridge { .. } = flow && !captured_query {
        if !services::is_room_collection(capability) { targets.extend(target.map(str::to_owned)); }
    } else {
        // A worker has already captured the outgoing query. Unlike the initial
        // collection consent, approval cannot change the rooms in that query.
        if capability.id == "matrix.rooms.messages.search" && let Some(ids) = parameters["room_ids"].as_array() {
            targets.extend(ids.iter().filter_map(|id| id.as_str().map(str::to_owned)));
        }
        targets.extend(target.map(str::to_owned));
    }
    (Some(scope), scope_targets(state, targets))
}

fn resume_flow(cx: &mut Cx, ui: &WidgetRef, flow: FlowContinuation) {
    match flow {
        FlowContinuation::Worker(worker) => worker.finish(Ok(())),
        FlowContinuation::Bridge { request, .. } => replay_bridge_request(cx, ui, request),
        FlowContinuation::Generated { review } => {
            let pending = with_a2app(|state| state.pending_generated.take_if(|pending| pending.matches_review(&review))).flatten();
            if let Some(pending) = pending { finish_generated_app(cx, ui, pending); }
            else { let _ = a2app_core::information_flow::cancel_effect(review.id); }
        }
    }
}

/// The verb phrase a permission prompt shows for one parked AI tool call —
/// what the AI wants to *do* ("read recent messages in this room"), not a
/// catalog noun ("Recent messages"), so the popup reads like the request it
/// answers.
#[cfg(unix)]
fn ai_prompt_action(
    rooms: Option<&RoomsListRef>,
    perm: Permission,
    parked: &[ParkedRequest],
) -> String {
    for p in parked {
        if let ParkedRequest::NetworkAccess { host, .. } = p {
            return format!("connect to “{host}”");
        }
        if let ParkedRequest::AiTool { room_id: _room_id, job } = p {
            return match job {
                SessionJob::ReadTool { kind, .. } => match kind {
                    ReadToolKind::Messages { .. } => String::from("read recent messages in this room"),
                    ReadToolKind::Older { .. } => String::from("read this room's older messages"),
                    ReadToolKind::Info => String::from("see this room's details"),
                    ReadToolKind::OtherRoom { room, .. } => {
                        format!("read messages in “{}”", resolve_room_label(rooms, room))
                    }
                    ReadToolKind::ListRooms => String::from("see a list of your rooms"),
                    ReadToolKind::ListSpaces => String::from("see your spaces"),
                    ReadToolKind::SpaceInfo { space } => {
                        format!("see details of the space “{}”", resolve_room_label(rooms, space))
                    }
                    ReadToolKind::SpaceRooms { space } => {
                        format!("see the rooms in the space “{}”", resolve_room_label(rooms, space))
                    }
                    // Ungated plumbing never parks behind a prompt; this arm
                    // exists for exhaustiveness.
                    ReadToolKind::Memory { .. } => String::from("recall its own past replies"),
                },
                SessionJob::LaunchSplashApp { .. } => String::from("build and run a mini-app"),
                SessionJob::ListApps { .. } => String::from("see which mini-apps you have installed"),
                // The prompt is built inside `with_a2app`, so it must not
                // re-enter to resolve the app's display name; the id is fine.
                SessionJob::LaunchApp { app_id, .. } => {
                    format!("run the mini-app \"{app_id}\"")
                }
                SessionJob::PostRoomMessage { room_id: room, .. } => {
                    format!("post a message into “{}”", resolve_room_label(rooms, room))
                }
                SessionJob::SendRoomMessage { .. } => continue,
                // A network-access job never reaches this arm (it parks as
                // `ParkedRequest::NetworkAccess` and is handled above).
                SessionJob::NetworkAccess { .. } | SessionJob::FetchUrl { .. } => continue,
                // An app-tool invocation names its tool directly.
                SessionJob::InvokeMiniAppTool { tool, .. } => {
                    format!("use the mini-app tool \"{tool}\"")
                }
                // A bridged call is the same request; name the app's tool.
                SessionJob::CallMiniAppTool { tool, .. } => {
                    format!("use the mini-app tool \"{tool}\"")
                }
                // Never parked (it answers immediately), but exhaustive.
                SessionJob::ListMiniAppTools { .. } => continue,
                SessionJob::RequestTaskPermissions { .. } => continue,
            };
        }
    }
    match perm {
        Permission::McpTools => String::from("use a tool a mini-app registered"),
        Permission::Network => String::from("connect to the internet"),
        Permission::MatrixRoomRead => String::from("read messages in this room"),
        Permission::MatrixRoomInfo => String::from("see this room's details"),
        Permission::MatrixRoomsRead => String::from("read messages in another room"),
        Permission::MatrixRoomsList => String::from("see a list of your rooms"),
        Permission::AppGeneration => String::from("build and run a mini-app"),
        _ => perm.title().to_string(),
    }
}

/// Why an AI tool is asking, shown under the prompt's action line.
#[cfg(unix)]
fn ai_prompt_reason(
    rooms: Option<&RoomsListRef>,
    perm: Permission,
    parked: &[ParkedRequest],
) -> String {
    for p in parked {
        if let ParkedRequest::NetworkAccess { host, .. } = p {
            return format!(
                "The AI wants to fetch from “{host}”. Choose which internet destinations \
                 it may reach and for how long."
            );
        }
        if let ParkedRequest::AiTool { room_id: _room_id, job } = p {
            return match job {
                SessionJob::ReadTool { kind, .. } => match kind {
                    ReadToolKind::Messages { .. } => String::from("It is catching up on this conversation to answer you."),
                    ReadToolKind::Older { .. } => String::from("It needs more of this conversation's history to answer you."),
                    ReadToolKind::Info => String::from("It wants to know which room it is in."),
                    ReadToolKind::OtherRoom { room, .. } => {
                        format!("You asked it to read “{}”, which is outside this room.", resolve_room_label(rooms, room))
                    }
                    ReadToolKind::ListRooms => String::from("It needs to know which rooms you have before it can offer to read one."),
                    ReadToolKind::ListSpaces => String::from("It needs to know which spaces you have before it can list the rooms inside one."),
                    ReadToolKind::SpaceInfo { space } => {
                        format!("It wants details of the space “{}” so it can tell you about it.", resolve_room_label(rooms, space))
                    }
                    ReadToolKind::SpaceRooms { space } => {
                        format!("It wants to see the rooms grouped under the space “{}” so it can tell you about them.", resolve_room_label(rooms, space))
                    }
                    // Ungated plumbing never parks behind a prompt; this arm
                    // exists for exhaustiveness.
                    ReadToolKind::Memory { .. } => String::from("It is recalling what it previously said in this room."),
                },
                SessionJob::LaunchSplashApp { .. } => String::from("It is building an app you asked for."),
                SessionJob::ListApps { .. } => String::from("It needs to know which mini-apps you have before it can run one."),
                // Built inside `with_a2app`: use the id, do not re-enter.
                SessionJob::LaunchApp { app_id, .. } => {
                    format!("You asked it to run the mini-app \"{app_id}\".")
                }
                SessionJob::PostRoomMessage { room_id: room, .. } => {
                    format!("It wants to post a message into “{}”, outside this room. It can only post there if you allow this room.", resolve_room_label(rooms, room))
                }
                SessionJob::SendRoomMessage { .. } => continue,
                // A network-access job never reaches this arm (handled above).
                SessionJob::NetworkAccess { .. } | SessionJob::FetchUrl { .. } => continue,
                SessionJob::InvokeMiniAppTool { tool, .. } => format!(
                    "You allowed this app to register \"{tool}\". The description you reviewed is \
                     the text the model sees; the app's answer is returned to the model."
                ),
                SessionJob::CallMiniAppTool { tool, .. } => format!(
                    "You allowed this app to register \"{tool}\". The description you reviewed is \
                     the text the model sees; the app's answer is returned to the model."
                ),
                // Never parked (it answers immediately), but exhaustive.
                SessionJob::ListMiniAppTools { .. } => continue,
                SessionJob::RequestTaskPermissions { .. } => continue,
            };
        }
    }
    match perm {
        Permission::McpTools => String::from(
            "The AI wants to call a tool this mini-app registered. Its description was shown \
             when you allowed it; allowing here lets the model invoke it.",
        ),
        Permission::Network => String::from("It needs to fetch something from the internet."),
        Permission::MatrixRoomRead => String::from("It is catching up on this conversation to answer you."),
        Permission::MatrixRoomInfo => String::from("It wants to know which room it is in."),
        Permission::MatrixRoomsRead => String::from("It needs to read a room outside this one."),
        Permission::MatrixRoomsList => String::from("It needs to know which rooms you have."),
        Permission::AppGeneration => String::from("It is building an app you asked for."),
        _ => String::from("It needs this to answer your messages in this room."),
    }
}

/// The host, not the requester, supplies the origin. A cross-room request
/// shows its actual target so consent cannot silently spread to other rooms.
fn parked_rooms(parked: &ParkedRequest) -> (Option<String>, Option<String>) {
    match parked {
        ParkedRequest::Bridge(Some(request)) => {
            let (_, room) = a2app_core::manifest::split_instance_tag(&request.app_tag);
            let origin = room.map(str::to_string);
            let args: serde_json::Value = serde_json::from_str(&request.args_json).unwrap_or_default();
            let target = permission_target_room(&request.service, &args, origin.as_deref());
            (origin, target)
        }
        ParkedRequest::Bridge(None) => (None, None),
        #[cfg(unix)]
        ParkedRequest::AiTool { room_id, job } => {
            let target = match job {
                SessionJob::PostRoomMessage { room_id, .. } => room_id.clone(),
                SessionJob::ReadTool { kind: ReadToolKind::OtherRoom { room, .. }, .. } => room.clone(),
                SessionJob::ReadTool { kind: ReadToolKind::SpaceInfo { space } | ReadToolKind::SpaceRooms { space }, .. } => space.clone(),
                _ => room_id.to_string(),
            };
            (Some(room_id.to_string()), Some(target))
        }
        #[cfg(unix)]
        ParkedRequest::NetworkAccess { room_id, .. } => (Some(room_id.to_string()), Some(room_id.to_string())),
    }
}

fn parked_can_enable_writes(state: &A2AppState, subject: &str, permission: Permission, parked: &ParkedRequest) -> bool {
    let Some(manifest) = state.registry.get(subject) else { return false };
    let (origin, target) = parked_rooms(parked);
    let context = PermissionContext { origin_room: origin.as_deref(), target_room: target.as_deref() };
    if let Some(capability) = parked_capability(parked) {
        return services::can_enable_writes(&state.permissions, manifest, capability, context);
    }
    if let ParkedRequest::Bridge(Some(request)) = parked {
        if request.service == "permissions.request" {
            let args: serde_json::Value = serde_json::from_str(&request.args_json).unwrap_or_default();
            return args["perm"].as_str().and_then(Permission::from_str) == Some(permission)
                && services::can_enable_permission_writes(&state.permissions, manifest, permission, context);
        }
    }
    false
}

fn parked_capability(parked: &ParkedRequest) -> Option<&'static a2app_core::capabilities::Capability> {
    match parked {
        ParkedRequest::Bridge(Some(request)) => {
            if request.service == "permissions.request" { return None; }
            if request.service == "events.subscribe" {
                let args: serde_json::Value = serde_json::from_str(&request.args_json).ok()?;
                return a2app_core::capabilities::for_hook(args["event"].as_str()?);
            }
            a2app_core::capabilities::for_service(&request.service)
        }
        ParkedRequest::Bridge(None) => None,
        #[cfg(unix)]
        ParkedRequest::NetworkAccess { .. } => a2app_core::capabilities::by_id("network.http"),
        #[cfg(unix)]
        ParkedRequest::AiTool { job, .. } => match job {
            SessionJob::ReadTool { kind, .. } => kind.capability(),
            SessionJob::PostRoomMessage { .. } => a2app_core::capabilities::by_id("matrix.rooms.message.send"),
            SessionJob::ListApps { .. } => a2app_core::capabilities::by_id(LIST_APPS_CAP_ID),
            SessionJob::LaunchApp { .. } => a2app_core::capabilities::by_id(LAUNCH_APP_CAP_ID),
            SessionJob::LaunchSplashApp { .. } => a2app_core::capabilities::by_id("apps.generate"),
            _ => None,
        },
    }
}

fn parked_network_url(parked: &ParkedRequest) -> Option<String> {
    match parked {
        #[cfg(unix)]
        ParkedRequest::AiTool { job: SessionJob::FetchUrl { url, .. }, .. } => Some(url.clone()),
        ParkedRequest::Bridge(Some(request)) if request.service == "network.http" => {
            serde_json::from_str::<serde_json::Value>(&request.args_json).ok()?
                .get("url")?.as_str().map(str::to_owned)
        }
        #[cfg(unix)]
        ParkedRequest::NetworkAccess { url, .. } => Some(url.clone()),
        _ => None,
    }
}

fn parked_can_allow_once(parked: &ParkedRequest) -> bool {
    match parked {
        ParkedRequest::Bridge(None) => false,
        ParkedRequest::Bridge(Some(request)) => !matches!(request.service.as_str(), "permissions.request" | "events.subscribe"),
        #[cfg(unix)]
        ParkedRequest::AiTool { .. } | ParkedRequest::NetworkAccess { .. } => true,
    }
}

/// Describe the user's task rather than the internal permission category.
fn permission_action(capability: &a2app_core::capabilities::Capability, args: &serde_json::Value) -> String {
    match capability.id {
        "matrix.room.messages.search" | "matrix.rooms.messages.search" =>
            args["query"].as_str().map(|query| format!("Search messages for {query:?}"))
                .unwrap_or_else(|| "Search messages".into()),
        "network.http" => if matches!(args["method"].as_str().unwrap_or("GET"), "GET" | "HEAD") {
            "Fetch a web page".into()
        } else { "Send a web request".into() },
        "device.clipboard.write" => "Copy text to the clipboard".into(),
        "device.clipboard.read" => "Read text from the clipboard".into(),
        "device.url.open" => "Open a link in your browser".into(),
        "host.nav.event" => "Open this message".into(),
        "host.nav.thread" => "Open this conversation thread".into(),
        "host.nav.user" => "View this person's profile".into(),
        "host.nav.room" | "host.nav.space" => "Open this room or space".into(),
        "matrix.room.message.send" | "matrix.rooms.message.send" => {
            if let Some(body) = args["body"].as_str() {
                let mut shown = body.chars().take(240).collect::<String>();
                if body.chars().count() > 240 { shown.push('…'); }
                format!("Post this message:\n{shown}")
            } else { "Post a message".into() }
        }
        "matrix.room.message.reply" | "matrix.room.thread.reply" => "Reply to a message".into(),
        "matrix.room.pin.set" => (if args["pinned"].as_bool() == Some(false) { "Unpin this message" } else { "Pin this message" }).into(),
        "matrix.room.favorite.set" => (if args["on"].as_bool() == Some(false) { "Remove this room from your favorites" } else { "Add this room to your favorites" }).into(),
        "matrix.room.low_priority.set" => (if args["on"].as_bool() == Some(false) { "Restore this room's normal priority" } else { "Mark this room as low priority" }).into(),
        "matrix.room.unread.set" => (if args["on"].as_bool() == Some(false) { "Clear this room's unread flag" } else { "Mark this room as unread" }).into(),
        "matrix.invites.respond" => (if args["accept"].as_bool() == Some(false) { "Decline this room invite" } else { "Accept this room invite" }).into(),
        "matrix.rooms.join" => (if args["operation"].as_str() == Some("knock") { "Ask to join this room" } else { "Join this room" }).into(),
        "matrix.room.typing.send" => (if args["typing"].as_bool() == Some(false) { "Stop showing you as typing" } else { "Show you as typing in this room" }).into(),
        "matrix.room.invite.send" => args["user_id"].as_str().map(|user| format!("Invite {user} to this room"))
            .unwrap_or_else(|| "Invite someone to this room".into()),
        "matrix.room.messages.read" | "matrix.rooms.messages.read" => "Read recent messages".into(),
        "matrix.room.info.read" => "Read this room's name and settings".into(),
        "matrix.room.pins.read" => "Read this room's pinned messages".into(),
        "matrix.room.threads.read" => "List conversations in this room".into(),
        "matrix.room.thread.read" => "Load replies to this conversation".into(),
        "matrix.room.event.read" => "Load this message".into(),
        "matrix.room.messages.paginate" => "Load older messages in this room".into(),
        "matrix.room.receipts.read" => "Read who has read this room's messages".into(),
        "matrix.room.unread.read" => "Read this room's unread count and flags".into(),
        "matrix.room.power_levels.read" => "Read your permissions in this room".into(),
        "matrix.room.successor.read" => "Check this room's upgrade notice".into(),
        "matrix.room.link.create" => "Create a link to this room or message".into(),
        "matrix.rooms.list" => "List your rooms".into(),
        "matrix.rooms.invites.list" => "List your room invites".into(),
        "matrix.spaces.list" => "List your spaces".into(),
        "matrix.space.info.read" => "Read this space's details".into(),
        "matrix.space.rooms.list" => "Load the rooms in this space".into(),
        "matrix.user.profile.read" => args["user_id"].as_str().map(|user| format!("Look up the profile of {user}"))
            .unwrap_or_else(|| "Look up this person's profile".into()),
        "on_room_message" => "Watch new messages in this room".into(),
        "on_room_members_changed" => "Watch who joins or leaves this room".into(),
        "on_room_pins_changed" => "Watch changes to this room's pinned messages".into(),
        "on_room_typing" => "Watch who is typing in this room".into(),
        "on_room_receipt" => "Watch who reads this room's messages".into(),
        "on_room_reaction" => "Watch reactions to messages in this room".into(),
        "on_room_message_changed" => "Watch edits and removals of messages in this room".into(),
        "matrix.room.members.read" => "Read the room's member list".into(),
        "matrix.profile.read" => "Read your account profile".into(),
        "matrix.account.device.read" => "Read this device’s name and verification status".into(),
        "matrix.account.info.read" => "Read your Matrix server and account settings links".into(),
        "on_unread_totals_changed" => "Watch changes to your unread message totals".into(),
        _ => capability.title.into(),
    }
}

/// Default to the rooms requested; the popup can widen or narrow this scope.
fn parked_scope(parked: &ParkedRequest) -> RoomScope {
    if let ParkedRequest::Bridge(Some(request)) = parked {
        if let Some(capability) = parked_capability(parked)
            && let Ok(args) = serde_json::from_str::<serde_json::Value>(&request.args_json)
        { return bridge_scope(request, capability, &args, parked_rooms(parked).1.as_deref()); }
    }
    let (_, target) = parked_rooms(parked);
    if parked_capability(parked).is_some_and(|cap| cap.scope == a2app_core::capabilities::Scope::Space)
        && let Some(space) = target.as_ref()
    { return RoomScope::Selection { rooms: Vec::new(), spaces: vec![space.clone()] }; }
    target.as_deref().map(RoomScope::room).unwrap_or(RoomScope::AllRooms)
}

fn bridge_scope(_request: &SplashHostRequest, capability: &a2app_core::capabilities::Capability, args: &serde_json::Value, target: Option<&str>) -> RoomScope {
    capability_scope(capability, args, target)
}

fn capability_scope(capability: &a2app_core::capabilities::Capability, args: &serde_json::Value, target: Option<&str>) -> RoomScope {
    if capability.id == "matrix.rooms.messages.search" && let Some(rooms) = args["room_ids"].as_array() && !rooms.is_empty() {
        return RoomScope::Selection { rooms: rooms.iter().filter_map(|room| room.as_str().map(str::to_owned)).collect(), spaces: Vec::new() };
    }
    if matches!(capability.id, "matrix.space.info.read" | "matrix.space.rooms.list" | "host.nav.space")
        && let Some(space) = args["space_id"].as_str().filter(|space| !space.is_empty())
    {
        return RoomScope::Selection { rooms: Vec::new(), spaces: vec![space.into()] };
    }
    if capability.scope == a2app_core::capabilities::Scope::Space && let Some(space) = target {
        return RoomScope::Selection { rooms: Vec::new(), spaces: vec![space.into()] };
    }
    if services::is_room_collection(capability) { return RoomScope::AllRooms; }
    target.map(RoomScope::room).unwrap_or(RoomScope::AllRooms)
}

fn capability_has_room_scope(capability: &a2app_core::capabilities::Capability) -> bool {
    matches!(capability.scope, a2app_core::capabilities::Scope::Room
        | a2app_core::capabilities::Scope::MultiRoom | a2app_core::capabilities::Scope::Space)
}

fn scope_targets(state: &A2AppState, ids: impl IntoIterator<Item = String>) -> Vec<(String, Vec<String>)> {
    let ancestry_is_current = state.policy_spaces_revision == matrix::spaces::policy_spaces_revision();
    ids.into_iter().collect::<std::collections::BTreeSet<_>>().into_iter().map(|room| {
        let ancestors = if ancestry_is_current {
            state.permissions.room_space_memberships().get(&room).into_iter().flatten().cloned().collect()
        } else { Vec::new() };
        (room, ancestors)
    }).collect()
}

fn parked_scope_targets(state: &A2AppState, parked: &[ParkedRequest]) -> Vec<(String, Vec<String>)> {
    // Collections are filtered to the selected scope by their workers. Fixed
    // requests must be covered before approval, so they cannot prompt in a loop.
    scope_targets(state, parked.iter().filter(|request| parked_requires_fixed_scope(request))
        .filter_map(|request| parked_rooms(request).1))
}

fn parked_requires_fixed_scope(request: &ParkedRequest) -> bool {
    if let Some(capability) = parked_capability(request) {
        return (capability_has_room_scope(capability) && !services::is_room_collection(capability))
            || capability.id == "mcp.tools.register";
    }
    match request {
        ParkedRequest::Bridge(Some(request)) => request.service == "permissions.request",
        #[cfg(unix)]
        ParkedRequest::AiTool { .. } => true,
        _ => false,
    }
}

fn prompt_room_scope(perm: Permission, parked: &[ParkedRequest]) -> Option<RoomScope> {
    let request = parked.first()?;
    let spatial = parked_capability(request).map(|capability| capability_has_room_scope(capability)
        || capability.id == "mcp.tools.register").unwrap_or_else(|| matches!(perm,
        Permission::MatrixRoomRead | Permission::MatrixRoomSend | Permission::MatrixRoomInfo
            | Permission::MatrixRoomWatch | Permission::MatrixRoomInteract | Permission::MatrixRoomAppData
            | Permission::MatrixRoomManage | Permission::MatrixRoomInvite | Permission::MatrixMedia
            | Permission::MatrixRoomsList | Permission::MatrixRoomsRead | Permission::MatrixRoomsSend
            | Permission::MatrixMembership | Permission::MatrixSpaces | Permission::RobrixComposer
            | Permission::McpTools | Permission::AppLaunch | Permission::AppGeneration));
    spatial.then(|| parked_scope(request))
}

fn chosen_scope_covers(state: &A2AppState, scope: &RoomScope, targets: &[(String, Vec<String>)]) -> bool {
    match scope {
        RoomScope::AllRooms => true,
        RoomScope::Selection { rooms, spaces } => (!rooms.is_empty() || !spaces.is_empty())
            && targets.iter().all(|(room, _)| {
                rooms.contains(room) || spaces.contains(room)
                    || (state.policy_spaces_revision == matrix::spaces::policy_spaces_revision()
                        && state.permissions.room_scope_matches(scope, room))
            }),
    }
}

fn refresh_permission_scope_targets(cx: &mut Cx, ui: &WidgetRef) {
    permission_batch::refresh_targets(cx, ui);
}

/// Builds what the prompt modal shows for one (subject, group, parked) tuple.
/// An AI room's agent subject is presented as the room's AI with the concrete
/// read it asked for; a mini-app keeps its registry identity and the app's own
/// declared reason.
#[cfg_attr(not(unix), allow(unused_variables))]
fn prompt_info_for(
    state: &A2AppState,
    rooms: Option<&RoomsListRef>,
    subject: &str,
    perm: Permission,
    parked: &[ParkedRequest],
    tool: Option<&PromptToolGrant>,
) -> PromptInfo {
    let (origin_room_id, room_id) = parked.first().map(parked_rooms).unwrap_or_default();
    let network_url = parked.iter().find_map(parked_network_url);
    let can_allow_once = parked.first().is_some_and(parked_can_allow_once);
    let tool_preview = tool.map(|t| ToolPreview {
        name: t.name.clone(),
        description: t.description.clone(),
        args: t.args.clone(),
    });
    #[cfg(unix)]
    if let Some(room) = agent_room_of(subject) {
        let room_name = rooms
            .and_then(|r| room_display_name(r, room))
            .unwrap_or_else(|| room.to_string());
        return PromptInfo {
            prompt_id: 0,
            app_name: format!("AI in {room_name}"),
            app_icon: String::from("🤖"),
            perm,
            reason: Some(ai_prompt_reason(rooms, perm, parked)),
            capability: Some(ai_prompt_action(rooms, perm, parked)),
            agent: true,
            tool: tool_preview,
            scope: prompt_room_scope(perm, parked),
            scope_targets: parked_scope_targets(state, parked),
            room_id, origin_room_id, network_url, can_allow_once, collection: parked.iter().filter_map(parked_capability).any(services::is_room_collection), enable_writes: false,
        };
    }
    let (app_name, app_icon, mut reason) = state.registry.get(subject)
        .map(|m| (m.name.clone(), m.icon.clone(), m.reason_for(perm).map(str::to_string)))
        .unwrap_or_else(|| (subject.to_string(), String::new(), None));
    // Name the exact ability that asked, not just its group; a parked
    // subscribe names the hook it is for. An mcp-tools registration names the
    // tool itself, which is what the user is reviewing.
    let capability = if perm == Permission::McpTools {
        Some(match tool {
            Some(t) => format!("register the tool \"{}\"", t.name),
            None => String::from("register an AI tool"),
        })
    } else {
        parked.iter().find_map(|p| match p {
            ParkedRequest::Bridge(request) => {
                let Some(request) = request else { return None };
                let hook_name = (request.service == "events.subscribe")
                    .then(|| serde_json::from_str::<serde_json::Value>(&request.args_json).ok())
                    .flatten()
                    .and_then(|args| args["event"].as_str().map(str::to_string));
                match hook_name {
                    Some(name) => a2app_core::capabilities::for_hook(&name),
                    None => a2app_core::capabilities::for_service(&request.service),
                }
            }
            #[cfg(unix)]
            ParkedRequest::AiTool { .. } => None,
            #[cfg(unix)]
            ParkedRequest::NetworkAccess { .. } => None,
        })
        .map(|c| {
            let args = parked.iter().find_map(|parked| if let ParkedRequest::Bridge(Some(request)) = parked {
                serde_json::from_str::<serde_json::Value>(&request.args_json).ok()
            } else { None }).unwrap_or_default();
            if matches!(c.id, "matrix.room.messages.search" | "matrix.rooms.messages.search") {
                reason = Some(if args["server"].as_bool() == Some(true) {
                    "Your search text is sent to your Matrix server for unencrypted rooms. Encrypted rooms are searched on this device.".into()
                } else { "Searches messages already on this device. Your search text stays in Robrix.".into() });
            }
            permission_action(c, &args)
        })
    };
    PromptInfo { prompt_id: 0, app_name, app_icon, perm, reason, capability, agent: false, tool: tool_preview, scope: prompt_room_scope(perm, parked), scope_targets: parked_scope_targets(state, parked), room_id, origin_room_id, network_url, can_allow_once,
        collection: parked.iter().filter_map(parked_capability).any(services::is_room_collection), enable_writes: false }
}

/// Answers one session tool call with a finished result, whatever variant it
/// is. A granted read answers when its worker fetch lands; refusals and
/// session-teardown errors answer here so a serve thread never hangs.
#[cfg(unix)]
fn answer_session_job(job: SessionJob, result: Result<String, String>) {
    match job {
        SessionJob::LaunchSplashApp { answer, .. }
        | SessionJob::ListApps { answer, .. }
        | SessionJob::LaunchApp { answer, .. }
        | SessionJob::SendRoomMessage { answer, .. }
        | SessionJob::ReadTool { answer, .. }
        | SessionJob::PostRoomMessage { answer, .. }
        | SessionJob::NetworkAccess { answer, .. }
        | SessionJob::FetchUrl { answer, .. }
        | SessionJob::InvokeMiniAppTool { answer, .. }
        | SessionJob::CallMiniAppTool { answer, .. }
        | SessionJob::ListMiniAppTools { answer }
        | SessionJob::RequestTaskPermissions { answer, .. } => {
            let _ = answer.send(result);
        }
    }
}

/// What an AI tool call is told when its room subject refused it — an `Err`
/// the model reads and can act on (tell the user, ask them to allow it),
/// never a hang and never a crash.
#[cfg(unix)]
fn ai_tool_refused_text(perm: Permission) -> String {
    format!(
        "The user did not allow the \"{}\" permission for this task. If it is \
         part of a task you are starting, call request_task_permissions with \
         this need so the user can decide once; otherwise tell the user what \
         you wanted to do and why.",
        perm.title()
    )
}

/// Prepare capability consent for the same request as the outgoing review.
///
/// The caller installs this snapshot only after IFC approval succeeds. Once
/// receipts reach async workers but disappear from the live permission store.
fn combined_permission_store(request: &SplashHostRequest, permission: Permission, answer: &PermissionPromptAction) -> Result<(PermissionStore, Option<(u64, bool)>), String> {
    let (once, duration) = match answer {
        PermissionPromptAction::AllowFlowOnce => (true, GrantDuration::RobrixSession),
        PermissionPromptAction::AllowFlowSession => (false, GrantDuration::RobrixSession),
        PermissionPromptAction::AllowFlow { duration } => (false, duration.clone()),
        PermissionPromptAction::AllowFlowScoped { duration, .. } => (false, *duration),
        _ => return Err("This request was not approved.".into()),
    };
    let context = super::information_flow::context_for_heap(request.heap_key)?;
    let args: serde_json::Value = serde_json::from_str(&request.args_json).map_err(|error| error.to_string())?;
    let capability = a2app_core::capabilities::for_service(&request.service).ok_or("Unknown app action.")?;
    let target = permission_target_room(&request.service, &args, context.room());
    let permission_context = PermissionContext { origin_room: context.room(), target_room: target.as_deref() };
    with_a2app(|state| {
        let subject = context.app().ok_or("Missing mini-app identity.")?;
        let manifest = state.registry.get(subject).ok_or("This mini-app was removed.")?;
        let enable_writes = services::can_enable_writes(&state.permissions, manifest, capability, permission_context);
        let effective = if services::is_room_collection(capability) {
            state.permissions.effective_collection_capability_in_context(manifest, capability, permission_context)
        } else { state.permissions.effective_capability_in_context(manifest, capability, permission_context) };
        if matches!(effective, Effective::Denied | Effective::Undeclared) && !enable_writes {
            return Err("This action is blocked by your app or room settings.".into());
        }
        if enable_writes && once { return Err("Enabling room changes needs a session or lasting approval.".into()); }
        let mut store = state.permissions.clone();
        if enable_writes { store.set_matrix_write(true); }
        let scope = match answer {
            PermissionPromptAction::AllowFlowScoped { scope, .. } => scope.clone(),
            _ => bridge_scope(request, capability, &args, permission_context.target_room),
        };
        if !services::is_room_collection(capability) && capability_has_room_scope(capability) {
            let targets = scope_targets(state, permission_context.target_room.map(str::to_owned));
            if !chosen_scope_covers(state, &scope, &targets) {
                return Err("Choose a room or space that includes the room used by this request.".into());
            }
        }
        let network = request.service == "network.http";
        let id = if network {
            let url = args["url"].as_str().ok_or("Missing website address.")?;
            let scope_network = if once { NetworkScope::ExactUrl(url.into()) }
                else { NetworkScope::from_url(url, a2app_core::permissions::NetworkScopeKind::Origin)? };
            store.allow_network(subject, scope_network, scope, duration, context.room())?
        } else {
            store.grant_scoped(subject, permission, Some(capability.id), scope, duration, context.room())?
        };
        if once {
            if network { store.mark_network_request_once(id); } else { store.mark_request_once(id); }
        }
        Ok((store, once.then_some((id, network))))
    }).unwrap_or_else(|| Err("Mini Apps are unavailable.".into()))
}

fn answer_permission_prompt(cx: &mut Cx, ui: &WidgetRef, response: PermissionPromptResponse) {
    // A grouped dialog must answer all displayed members together. Ignore a
    // stale singleton response instead of leaving its siblings unanswered.
    if with_a2app(|state| !state.permission_batch_busy && !state.active_prompt_batch.is_empty()).unwrap_or(false) { return; }
    if !with_a2app(|state| state.active_prompt.as_ref().is_some_and(|prompt| prompt.id == response.prompt_id)).unwrap_or(false) { return; }
    let answer = response.answer;
    ui.modal(cx, ids!(a2app_permission_modal)).close(cx);
    let Some(Some(mut prompt)) = with_a2app(|state| state.active_prompt.take()) else { return };

    if matches!(answer, PermissionPromptAction::None) {
        with_a2app(|state| state.active_prompt = Some(prompt));
        return;
    }
    if let Some(mut flow) = prompt.flow.take() {
        let subject = prompt.subject.clone();
        let permission = prompt.perm;
        if matches!(answer, PermissionPromptAction::NotNow | PermissionPromptAction::Deny) {
            permission_batch::dismiss_effect(flow.review());
        }
        let mut combined_once = None;
        let mut combined_permission = false;
        let result = flow.approve(|review| super::information_flow::current_context(&review.context)
            .and_then(|_| if !flow.can_prompt() {
                Err("This mini-app is no longer open. Reopen it to continue.".into())
            } else {
                if !matches!(answer, PermissionPromptAction::AllowFlowOnce | PermissionPromptAction::AllowFlowSession | PermissionPromptAction::AllowFlow { .. } | PermissionPromptAction::AllowFlowScoped { .. }) {
                    return Err("This request was not approved.".into());
                }
                let permission = match &flow {
                    FlowContinuation::Bridge { request, permission: Some(permission), .. } =>
                        Some(combined_permission_store(request, *permission, &answer)?),
                    _ => None,
                };
                let approved = if review.allowed { Ok(()) } else { match answer {
                    PermissionPromptAction::AllowFlowOnce if flow.allow_once() => a2app_core::information_flow::approve_effect_once(review),
                    PermissionPromptAction::AllowFlowSession => a2app_core::information_flow::approve_effect_session(review, a2app_core::information_flow::SharingDuration::RobrixSession),
                    PermissionPromptAction::AllowFlow { duration: GrantDuration::RobrixSession } => a2app_core::information_flow::approve_effect_session(review, a2app_core::information_flow::SharingDuration::RobrixSession),
                    PermissionPromptAction::AllowFlow { duration: GrantDuration::Always } => a2app_core::information_flow::approve_effect_always(review),
                    PermissionPromptAction::AllowFlowScoped { ref scope, duration } => {
                        let duration = match duration {
                            GrantDuration::RobrixSession => a2app_core::information_flow::SharingDuration::RobrixSession,
                            GrantDuration::Always => a2app_core::information_flow::SharingDuration::Permanent,
                            GrantDuration::RoomSession => return Err("Choose Until you quit Robrix or Forever for this approval.".into()),
                        };
                        a2app_core::information_flow::approve_effect_scoped(review, scope.clone(), duration)
                    }
                    _ => Err("This request was not approved.".into()),
                }};
                approved?;
                if let Some((store, once)) = permission {
                    with_a2app(|state| { state.permissions = store; state.perms_dirty = true; });
                    combined_once = once;
                    combined_permission = true;
                    publish_grants(cx);
                }
                Ok(())
            }));
        match result {
            Ok(()) => resume_flow(cx, ui, flow),
            Err(error) => {
                // Data can arrive while the review is open. Keep the exact
                // operation waiting and require a fresh answer for new sources.
                let updated = matches!(answer, PermissionPromptAction::AllowFlowOnce | PermissionPromptAction::AllowFlowSession | PermissionPromptAction::AllowFlow { .. } | PermissionPromptAction::AllowFlowScoped { .. })
                    && !flow.is_cancelled() && refresh_flow_capture(&mut flow).unwrap_or(false);
                if updated && flow.review().allowed && flow_bridge_permission_granted(&flow) { resume_flow(cx, ui, flow); }
                else if updated {
                    prompt.id = next_permission_prompt_id();
                    prompt.flow = Some(flow);
                    with_a2app(|state| state.prompts.push_front(prompt));
                } else { refuse_flow(cx, flow, &error); }
            }
        }
        if let Some((id, network)) = combined_once {
            with_a2app(|state| { if network { state.permissions.remove_network_grant(id); } else { state.permissions.remove_scoped_grant(id); } });
            publish_grants(cx);
        }
        if combined_permission { apply_permission_to_running(cx, ui, &subject, permission); }
        publish_grants(cx);
        show_next_permission_prompt(cx, ui);
        ui.redraw(cx);
        return;
    }
    let setup = prompt.setup;
    let parked_count = prompt.parked.len();
    retain_live_bridge_requests(&mut prompt);
    if prompt.parked.is_empty() || prompt.parked.len() != parked_count {
        // An answer belongs to the displayed batch. Retired requests cannot
        // grant authority or redirect that answer onto a surviving request.
        if let Some(key) = setup {
            permission_batch::complete_setup(cx, key, prompt.id, false);
        } else if !prompt.parked.is_empty() {
            prompt.id = next_permission_prompt_id();
            with_a2app(|state| state.prompts.push_front(prompt));
        }
        show_next_permission_prompt(cx, ui);
        ui.redraw(cx);
        return;
    }
    let (origin, _) = prompt.parked.first().map(parked_rooms).unwrap_or_default();
    let network_url = prompt.parked.iter().find_map(parked_network_url);
    let capability = prompt.parked.iter().find_map(parked_capability).map(|cap| cap.id);
    let once = matches!(answer, PermissionPromptAction::AllowOnce);
    let mut temporary_grant = None;
    let mut temporary_network = None;
    let granted = match answer {
        PermissionPromptAction::AllowScoped { scope, duration, network } => {
            let result = with_a2app(|state| {
                if prompt.setup.is_some() {
                    let manifest = state.registry.get(&prompt.subject).ok_or("This mini-app is no longer installed.")?;
                    let mut permissions = state.permissions.clone();
                    if prompt.enable_writes { permissions.set_matrix_write(true); }
                    let context = PermissionContext { origin_room: origin.as_deref(), target_room: origin.as_deref() };
                    if matches!(services::permission_setup_status(&permissions, manifest, prompt.perm, context), Effective::Denied | Effective::Undeclared) {
                        return Err("This feature is blocked by your current permission settings. Change those settings before trying again.".into());
                    }
                }
                let targets = if prompt.setup.is_some() && matches!(prompt.perm,
                    Permission::MatrixRoomsList | Permission::MatrixRoomsRead | Permission::MatrixRoomsSend | Permission::MatrixSpaces)
                { Vec::new() } else { parked_scope_targets(state, &prompt.parked) };
                if !chosen_scope_covers(state, &scope, &targets) {
                    return Err("Choose rooms or spaces that include every room used by this request.".into());
                }
                if prompt.enable_writes && !state.permissions.matrix_write() {
                    let can_enable = prompt.parked.iter().all(|parked|
                        parked_can_enable_writes(state, &prompt.subject, prompt.perm, parked));
                    if !can_enable { return Err("Room or app permissions changed. Review them before enabling room changes.".into()); }
                }
                let result = if let Some(url) = &network_url {
                    state.permissions.allow_network(&prompt.subject,
                        network.unwrap_or_else(|| NetworkScope::ExactUrl(url.clone())),
                        scope, duration, origin.as_deref())
                } else if let Some(tool) = &prompt.tool {
                    state.permissions.grant_scoped_tool(&prompt.subject, &tool.full_name,
                        &tool.content_hash, scope, duration, origin.as_deref())
                } else {
                    state.permissions.grant_scoped(&prompt.subject, prompt.perm, capability,
                        scope, duration, origin.as_deref())
                };
                if result.is_ok() && prompt.enable_writes { state.permissions.set_matrix_write(true); }
                state.perms_dirty = true;
                result
            }).unwrap_or_else(|| Err("Permissions are unavailable.".into()));
            match result {
                Ok(_) => true,
                Err(error) => { enqueue_popup_notification(error, PopupKind::Error, Some(5.0)); false }
            }
        }
        PermissionPromptAction::AllowOnce => {
            // Temporarily authorize exactly this replay. No durable or
            // session grant survives it, and queued requests have separate prompts.
            let scope = prompt.parked.first().map(parked_scope).unwrap_or(RoomScope::AllRooms);
            prompt.parked.first().is_some_and(parked_can_allow_once) && with_a2app(|state| {
                if let Some(url) = &network_url {
                    temporary_network = state.permissions.allow_network(&prompt.subject,
                        NetworkScope::ExactUrl(url.clone()), scope, GrantDuration::RobrixSession,
                        origin.as_deref()).ok();
                    temporary_network.is_some()
                } else if let Some(tool) = &prompt.tool {
                    temporary_grant = state.permissions.grant_scoped_tool(&prompt.subject,
                        &tool.full_name, &tool.content_hash, scope, GrantDuration::RobrixSession,
                        origin.as_deref()).ok();
                    temporary_grant.is_some()
                } else {
                    temporary_grant = state.permissions.grant_scoped(&prompt.subject,
                        prompt.perm, capability, scope, GrantDuration::RobrixSession,
                        origin.as_deref()).ok();
                    temporary_grant.is_some()
                }
            }).unwrap_or(false)
        }
        PermissionPromptAction::Deny => {
            with_a2app(|state| {
                state.permissions.set(&prompt.subject, prompt.perm, GrantState::Denied);
                state.perms_dirty = true;
            });
            false
        }
        PermissionPromptAction::NotNow => {
            permission_batch::forget_gesture(&prompt.subject);
            with_a2app(|state| {
                if let Some(url) = &network_url {
                    if let Ok(url) = url::Url::parse(url)
                        && let Some(host) = url.host_str()
                    {
                        state.dismissed_net_hosts.insert((prompt.subject.clone(), host.to_string()));
                    }
                } else {
                    state.dismissed_prompts.insert((prompt.subject.clone(), prompt.perm));
                }
            });
            false
        }
        PermissionPromptAction::AllowFlowOnce | PermissionPromptAction::AllowFlowSession | PermissionPromptAction::AllowFlow { .. } | PermissionPromptAction::AllowFlowScoped { .. } => false,
        PermissionPromptAction::None => unreachable!(),
    };
    if let Some(id) = temporary_grant {
        with_a2app(|state| state.permissions.mark_request_once(id));
    }
    if let Some(id) = temporary_network {
        with_a2app(|state| state.permissions.mark_network_request_once(id));
    }
    publish_grants(cx);

    let subject = prompt.subject.clone();
    let perm = prompt.perm;
    if once && prompt.parked.len() > 1 {
        let rest = prompt.parked.split_off(1);
        with_a2app(|state| state.prompts.push_front(PermissionPrompt {
            setup: None, id: next_permission_prompt_id(),
            subject: subject.clone(), perm, parked: rest, tool: prompt.tool.clone(), flow: None,
            enable_writes: prompt.enable_writes, activations: prompt.activations.clone(),
        }));
    }
    // Replay or refuse everything parked behind this prompt.
    let activations = prompt.activations;
    for parked in prompt.parked {
        if setup.is_some() { continue; }
        match parked {
            ParkedRequest::Bridge(request) => {
                if let Some(request) = request.as_ref()
                    && !bridge_activation_is_live(request, &activations)
                { continue; }
                if granted {
                    if let Some(request) = request {
                        let asks = with_a2app(|state| {
                            let A2AppState { broker, registry, permissions, foreground_app, .. } = state;
                            let is_docked = |app_id: &str| instances::is_foreground(app_id);
                            let desktop_view = effective_is_desktop(cx);
                            let rooms = cx.has_global::<RoomsListRef>().then(|| cx.get_global::<RoomsListRef>().clone());
                            let room_name = |id: &str| room_display_name(rooms.as_ref()?, id);
                            broker.dispatch_after_grant(cx, BrokerCtx {
                                registry,
                                permissions,
                                foreground_app: foreground_app.as_deref(),
                                is_docked: &is_docked,
                                is_running: &instances::is_running,
                                pane_state: &instances::pane_state,
                                storage_path: &compartment_storage_path,
                                room_name: &room_name,
                                permission_target_room: Some(&permission_target_room),
                                desktop_view,
                                check_flow: &super::information_flow::check_request,
                                check_response: &super::information_flow::check_response,
                            }, request)
                        }).unwrap_or_default();
                        for ask in asks {
                            apply_broker_ask(cx, ui, ask);
                        }
                    }
                } else if let Some(request) = request {
                    with_a2app(|state| state.broker.declined(&request));
                    Broker::respond_denied(cx, &request);
                }
            }
            #[cfg(unix)]
            ParkedRequest::AiTool { room_id, job } => {
                if granted {
                    execute_session_job(cx, ui, &room_id, job);
                } else {
                    let reason = ai_tool_refused_text(perm);
                    note_ai_tool_call(&room_id, &ai_job_tool_name(&job), false, &reason);
                    answer_session_job(job, Err(reason));
                }
            }
            #[cfg(unix)]
            ParkedRequest::NetworkAccess { room_id, host, url, answer, .. } => {
                let allowed = granted && with_a2app(|state| state.permissions.is_url_allowed(&subject, &url,
                    PermissionContext { origin_room: Some(room_id.as_str()), target_room: Some(room_id.as_str()) })).unwrap_or(false);
                let _ = if allowed {
                    answer.send(Ok(String::from("allowed")))
                } else {
                    answer.send(Err(format!(
                        "The user did not allow the AI in this room to reach `{host}`. \
                         Tell them what you wanted to fetch and why, so they can allow it."
                    )))
                };
            }
        }
    }

    if once {
        with_a2app(|state| {
            if let Some(id) = temporary_grant { state.permissions.remove_scoped_grant(id); }
            if let Some(id) = temporary_network { state.permissions.remove_network_grant(id); }
        });
        publish_grants(cx);
    }
    if let Some(key) = setup { permission_batch::complete_setup(cx, key, prompt.id, granted); }
    if !is_agent_subject(&subject) && setup.is_none() {
        apply_permission_to_running(cx, ui, &subject, perm);
    }
    show_next_permission_prompt(cx, ui);
    ui.redraw(cx);
}

fn refresh_flow_capture(flow: &mut FlowContinuation) -> Result<bool, String> {
    let review = flow.review();
    super::information_flow::current_context(&review.context)?;
    if !flow.can_prompt() { return Ok(false); }
    let payload = serde_json::from_str(&review.payload).map_err(|_| "This permission request is invalid.".to_owned())?;
    let updated = a2app_core::information_flow::prepare_effect_for_activation(
        &review.context, review.epoch, review.recipient.as_ref(), review.action.as_ref(), &payload)?;
    if updated == *review { return Ok(false); }
    if review.id != updated.id { let _ = a2app_core::information_flow::cancel_effect(review.id); }
    flow.replace_review(updated);
    Ok(true)
}

/// Handles one `request_task_permissions` tool call: parse the untrusted
/// request, resolve it into the exact plan Robrix will show, and either park
/// it behind the task modal or answer at once when nothing needs the user.
#[cfg(unix)]
fn run_request_task_permissions(    cx: &mut Cx,
    ui: &WidgetRef,
    room_id: &OwnedRoomId,
    request: serde_json::Value,
    answer: Sender<Result<String, String>>,
) {
    let context = match super::information_flow::agent_context(room_id.as_str()) {
        Ok(context) => context,
        Err(error) => { let _ = answer.send(Err(error)); return; }
    };
    // Cap the number of requests a turn can make, so a looping agent cannot
    // keep raising the modal. Every call counts, including malformed ones.
    let over_limit = with_a2app(|state| {
        !count_task_request(&mut state.task_request_counts, room_id.as_str())
    })
    .unwrap_or(true);
    if over_limit {
        let message = String::from(
            "This turn has already made three permission requests, so no more will be shown. \
             Do as much as you can with what you have and explain in your reply what you could not do.",
        );
        note_ai_tool_call(room_id, "request_task_permissions", false, "Request limit reached");
        let _ = answer.send(Err(message));
        return;
    }
    let request = match task_grants::parse_request(&request) {
        Ok(request) => request,
        Err(error) => { let _ = answer.send(Err(error)); return; }
    };
    let account = context.account().to_string();
    let subject = agent_subject(room_id.as_str());
    let epoch = match a2app_core::information_flow::context_epoch(&context) {
        Ok(epoch) => epoch,
        Err(error) => { let _ = answer.send(Err(error)); return; }
    };
    let (store, model_recipient, task_id) = with_a2app(|state| {
        state.next_task_id = state.next_task_id.saturating_add(1);
        let recipient = a2app_agent::model_transport::current_recipient(&state.agent_prefs)
            .ok()
            .map(|recipient| Recipient::ModelProvider(recipient.id));
        (state.permissions.clone(), recipient, state.next_task_id)
    })
    .unwrap_or_else(|| (PermissionStore::default(), None, 0));
    // The AI room's own reply/activity rows are unencrypted Matrix state, so
    // every source the task reads must also be allowed to this origin.
    let homeserver_recipient = crate::sliding_sync::get_client()
        .and_then(|client| Recipient::network_origin(client.homeserver().as_str()).ok());
    let joined = |id: &str| {
        let Some(client) = crate::sliding_sync::get_client() else { return false };
        let Ok(room) = OwnedRoomId::try_from(id) else { return false };
        client.get_room(&room).is_some()
    };
    // Seal the same room-local, unambiguous name the invocation will check.
    let app_tool_name = |tool: &str| {
        with_a2app(|state| canonical_app_tool_name(state, room_id, tool)).flatten()
    };
    let inputs = ResolveInputs {
        task_id,
        subject: &subject,
        account: &account,
        room: room_id.as_str(),
        context: &context,
        epoch,
        model_recipient,
        homeserver_recipient,
        joined: &joined,
        declared_capabilities: AI_ROOM_SESSION_CAP_IDS,
        app_tool_name: &app_tool_name,
        store: &store,
        flow: &task_grants::GlobalFlow,
    };
    let plan = match task_grants::resolve(&request, &inputs) {
        Ok(plan) => plan,
        Err(error) => { let _ = answer.send(Err(error)); return; }
    };
    let declined = |plan: &TaskPlan| task_grants::outcome(plan, &std::collections::BTreeSet::new()).to_string();
    if !plan.needs_prompt() {
        // Every item is already allowed or blocked by policy: no modal, so
        // the agent is answered at once and blocked items surface on the turn.
        let (ok, note) = settled_task_receipt(&plan);
        note_ai_tool_call(room_id, "request_task_permissions", ok, note);
        let _ = answer.send(Ok(declined(&plan)));
        return;
    }
    let dismissed = with_a2app(|state| {
        task_plan_is_dismissed(&state.dismissed_task_plans, room_id.as_str(), &plan.need_keys())
    })
    .unwrap_or(false);
    if dismissed {
        note_ai_tool_call(room_id, "request_task_permissions", false, "Declined this turn");
        let _ = answer.send(Ok(declined(&plan)));
        return;
    }
    with_a2app(|state| {
        state.task_prompts.push_back(TaskPrompt { room_id: room_id.clone(), plan, resume: TaskResume::AgentTool(answer) });
    });
    show_next_permission_prompt(cx, ui);
}

/// The tool-call receipt for a plan that needed no prompt: success only when
/// every item is already allowed, a failed block receipt when every
/// item is blocked, a failed "Not offered" when every item is
/// unofferable, and a failed mixed note otherwise.
#[cfg(unix)]
fn settled_task_receipt(plan: &TaskPlan) -> (bool, &'static str) {
    let all = |pred: fn(&ItemState) -> bool| !plan.items.is_empty() && plan.items.iter().all(|item| pred(&item.state));
    if all(|state| matches!(state, ItemState::AlreadyAllowed)) {
        (true, "Nothing new to allow")
    } else if all(|state| matches!(state, ItemState::Blocked(TaskReason::BlockedByRoomPolicy))) {
        (false, "Blocked by room policy")
    } else if all(|state| matches!(state, ItemState::Blocked(_))) {
        (false, "Blocked by permission settings")
    } else if all(|state| matches!(state, ItemState::NotOffered(_))) {
        (false, "Not offered")
    } else {
        (false, "Some needs were refused")
    }
}

/// Records one task request for `room` and returns whether it is within the
/// per-turn cap. Every call counts, including malformed ones.
#[cfg(unix)]
fn count_task_request(counts: &mut HashMap<String, u32>, room: &str) -> bool {
    let count = counts.entry(room.to_string()).or_insert(0);
    *count += 1;
    *count <= 3
}

/// Whether every need was already declined in this room during the turn.
/// Combining earlier declined requests must not open another permission modal.
#[cfg(unix)]
fn task_plan_is_dismissed(
    dismissed: &HashMap<(String, [u8; 32]), BTreeSet<String>>,
    room: &str,
    needs: &BTreeSet<String>,
) -> bool {
    let previous: Vec<_> = dismissed
        .iter()
        .filter(|((dismissed_room, _), _)| dismissed_room == room)
        .map(|(_, dismissed_needs)| dismissed_needs)
        .collect();
    !previous.is_empty() && needs.iter().all(|need| previous.iter().any(|declined| declined.contains(need)))
}

/// Shows the next task prompt, if none is already open. Task prompts are
/// shown ahead of single-permission prompts, one modal at a time.
#[cfg(unix)]
fn show_next_task_prompt(cx: &mut Cx, ui: &WidgetRef) {
    // Names for the detail lines come from the room list, so a prompt says
    // "General" rather than `!abc:server`.
    let rooms = cx.has_global::<RoomsListRef>().then(|| cx.get_global::<RoomsListRef>().clone());
    let info = with_a2app(|state| {
        if permission_modal_is_open(state) {
            return None;
        }
        let prompt = state.task_prompts.pop_front()?;
        // The model label and the room's live app-tool names let the details
        // name what a grant reaches instead of echoing raw ids.
        let model = a2app_agent::model_transport::current_recipient(&state.agent_prefs)
            .ok()
            .map(|recipient| (recipient.id, recipient.label));
        let tool_names: Vec<(String, String)> = state
            .app_tools
            .values()
            .map(|reg| (reg.full_name.clone(), reg.raw_name.clone()))
            .collect();
        let info = task_prompt_info(&prompt.plan, rooms.as_ref(), model.as_ref(), &tool_names);
        state.active_task = Some(prompt);
        Some(info)
    })
    .flatten();
    let Some(info) = info else { return };
    ui.task_permission_prompt(cx, ids!(task_permission_modal.content)).show(cx, &info);
    ui.modal(cx, ids!(task_permission_modal)).open(cx);
}

/// Shows the next parked exact-action review, if none is already open. Reviews
/// are shown ahead of task plans, one modal at a time.
#[cfg(unix)]
fn show_next_exact_review(cx: &mut Cx, ui: &WidgetRef) {
    let info = with_a2app(|state| {
        if permission_modal_is_open(state) {
            return None;
        }
        let prompt = state.exact_reviews.pop_front()?;
        let info = prompt.info.clone();
        state.active_exact_review = Some(prompt);
        Some(info)
    })
    .flatten();
    let Some(info) = info else { return };
    ui.task_permission_prompt(cx, ids!(task_permission_modal.content)).show(cx, &info);
    ui.modal(cx, ids!(task_permission_modal)).open(cx);
}

/// Builds what the modal shows: the agent's verbatim paragraph and one row per
/// exact item. Robrix's item titles and details are authoritative; the agent's
/// text is context only. Room/space ids in those rows are resolved to names,
/// and the model/app-tool ids to their live labels, so the user reads what a
/// grant reaches rather than raw identifiers.
#[cfg(unix)]
fn task_prompt_info(
    plan: &TaskPlan,
    rooms: Option<&RoomsListRef>,
    model: Option<&(String, String)>,
    tool_names: &[(String, String)],
) -> TaskPromptInfo {
    let name_of = |id: &str| resolve_room_label(rooms, id);
    let model_label = model.map(|(id, label)| (id.as_str(), label.as_str()));
    let tool_name_of = |tool: &str| {
        tool_names
            .iter()
            .find(|(full_name, _)| full_name == tool)
            .or_else(|| tool_names.iter().find(|(_, raw_name)| raw_name == tool))
            .map(|(_, raw_name)| raw_name.clone())
    };
    let items = plan
        .items
        .iter()
        .map(|item| TaskItemView {
            id: item.id.clone(),
            title: task_item_title(&item.action, &name_of, model_label, &tool_name_of),
            detail: task_item_detail(&item.action, &name_of, model_label, &tool_name_of),
            chip: task_item_chip(&item.state),
            grantable: item.state.is_grantable(),
            checked: item.state.is_grantable(),
        })
        .collect();
    let risk = plan
        .items
        .iter()
        .any(|item| item.state.is_grantable() && item.risk >= a2app_core::capabilities::Risk::High)
        .then(|| "This plan includes broad or high-risk access. Review the details before allowing.".to_string());
    TaskPromptInfo {
        // Only the assistant's own paragraph. Robrix's framing is the modal
        // title and the detail rows below, never a label on the agent's words.
        explanation: plan.explanation.clone(),
        items,
        risk,
    }
}

/// The headline for one plan item, with ids resolved to the names the user
/// recognizes.
#[cfg(unix)]
fn task_item_title(
    action: &a2app_core::task_grants::PlanAction,
    name_of: &impl Fn(&str) -> String,
    model: Option<(&str, &str)>,
    tool_name_of: &impl Fn(&str) -> Option<String>,
) -> String {
    use a2app_core::task_grants::PlanAction;
    match action {
        PlanAction::Scoped { capability, .. } => a2app_core::capabilities::by_id(capability)
            .map(|cap| cap.title.to_string())
            .unwrap_or_else(|| capability.clone()),
        PlanAction::Network { url, .. } => format!("Open {url}"),
        PlanAction::Tool { tool, .. } => format!(
            "Use the mini-app tool \u{201c}{}\u{201d}",
            tool_name_of(tool).as_deref().unwrap_or(tool),
        ),
        PlanAction::Flow { source, recipient } => format!(
            "Let {} reach {}",
            task_source_label(source, name_of),
            task_recipient_label(recipient, name_of, model),
        ),
    }
}

/// A flow's source, named for the user (room/space ids resolved, account and
/// the directory described rather than dumped).
#[cfg(unix)]
fn task_source_label(source: &Source, name_of: &impl Fn(&str) -> String) -> String {
    match source {
        Source::Room { room, .. } => format!("\u{201c}{}\u{201d}", name_of(room)),
        Source::Account { account } => format!("your account ({account})"),
        Source::RoomDirectory { .. } => "your room directory".to_string(),
        Source::UnknownPrivate => "stored data".to_string(),
    }
}

/// A flow's recipient, named for the user (rooms resolved, the model named by
/// its current label, origins kept since that is what the grant opens).
#[cfg(unix)]
fn task_recipient_label(
    recipient: &Recipient,
    name_of: &impl Fn(&str) -> String,
    model: Option<(&str, &str)>,
) -> String {
    match recipient {
        Recipient::MatrixRoom { room, .. } => format!("room \u{201c}{}\u{201d}", name_of(room)),
        Recipient::ModelProvider(provider) => match model {
            Some((id, label)) if id == provider && !label.is_empty() => format!("your AI service ({label})"),
            _ => "your AI service".to_string(),
        },
        Recipient::NetworkOrigin(origin) => origin.clone(),
        Recipient::External => "outside Robrix".to_string(),
        Recipient::Clipboard => "the clipboard".to_string(),
    }
}

/// The detail line for one plan item, with room/space ids resolved to the
/// names the user recognizes and app-tool ids to the app's own tool name. A
/// target unknown to the room list falls back to its id so the row is never
/// empty.
#[cfg(unix)]
fn task_item_detail(
    action: &a2app_core::task_grants::PlanAction,
    name_of: &impl Fn(&str) -> String,
    model: Option<(&str, &str)>,
    tool_name_of: &impl Fn(&str) -> Option<String>,
) -> String {
    use a2app_core::task_grants::PlanAction;
    match action {
        PlanAction::Scoped { scope, .. } => match scope {
            RoomScope::AllRooms => "all rooms".to_string(),
            // The row's title already names the operation ("Messages in another
            // room"), so the detail is just the resolved targets — adding
            // "rooms:"/"spaces:" here duplicated the noun and disagreed about
            // singular/plural.
            RoomScope::Selection { rooms, spaces } => rooms
                .iter()
                .chain(spaces.iter())
                .map(|id| name_of(id))
                .collect::<Vec<_>>()
                .join(", "),
        },
        PlanAction::Flow { source, recipient } => format!(
            "{} \u{2192} {}",
            task_source_label(source, name_of),
            task_recipient_label(recipient, name_of, model),
        ),
        PlanAction::Tool { tool, .. } => tool_name_of(tool)
            .map(|name| format!("tool \u{201c}{name}\u{201d}"))
            .unwrap_or_default(),
        // A URL is already exactly the thing the grant opens.
        PlanAction::Network { .. } => action.detail().unwrap_or_default(),
    }
}

#[cfg(unix)]
fn task_item_chip(state: &ItemState) -> String {
    match state {
        ItemState::AlreadyAllowed => "Already allowed".to_string(),
        ItemState::NeedsGrant => "Will be granted until this turn ends".to_string(),
        ItemState::Blocked(reason) => format!("Blocked: {}", task_reason_label(*reason)),
        ItemState::NotOffered(reason) => format!("Not offered: {}", task_reason_label(*reason)),
    }
}

#[cfg(unix)]
fn task_reason_label(reason: TaskReason) -> &'static str {
    match reason {
        TaskReason::Declined => "the user declined",
        TaskReason::DeclinedDependency => "a sharing rule it needed was left unchecked",
        TaskReason::BlockedByRoomPolicy => "a room's protection blocks it",
        TaskReason::BlockedByPermission => "blocked by permission settings",
        TaskReason::NotOffered => "not offered here",
        TaskReason::InvalidTarget => "not a room or space you have joined",
        TaskReason::AlreadyAllowed => "already allowed",
    }
}

/// Takes the permission store out of the runtime state for an operation that
/// must not run while the state is borrowed (a task apply, which also writes
/// the information-flow registry). Restores it on drop, so a panic or an
/// early return never loses the store.
#[cfg(unix)]
struct PermissionStoreGuard {
    store: Option<PermissionStore>,
}

#[cfg(unix)]
impl PermissionStoreGuard {
    fn take() -> Self {
        Self { store: with_a2app(|state| std::mem::take(&mut state.permissions)) }
    }

    fn store(&mut self) -> &mut PermissionStore {
        self.store.as_mut().expect("the permission store guard was already restored")
    }

    /// Restores the store and runs `after` with it back in the state.
    fn restore(mut self, after: impl FnOnce(&mut A2AppState)) {
        let store = self.store.take().expect("the permission store guard was already restored");
        with_a2app(|state| {
            state.permissions = store;
            state.mark_perms_dirty();
            after(state);
        });
    }
}

#[cfg(unix)]
impl Drop for PermissionStoreGuard {
    fn drop(&mut self) {
        if let Some(store) = self.store.take() {
            with_a2app(|state| {
                state.permissions = store;
                state.mark_perms_dirty();
            });
        }
    }
}

/// Applies the user's answer to one task prompt. "Not now" is remembered by
/// plan hash until the turn closes; Allow applies exactly the checked items as
/// one atomic, turn-scoped batch.
#[cfg(unix)]
fn answer_task_prompt(cx: &mut Cx, ui: &WidgetRef, action: TaskPermissionAction) {
    // An exact-action review uses the same modal; handle it first.
    if let Some(review) = with_a2app(|state| state.active_exact_review.take()).flatten() {
        ui.modal(cx, ids!(task_permission_modal)).close(cx);
        if matches!(action, TaskPermissionAction::None) {
            with_a2app(|state| state.active_exact_review = Some(review));
            return;
        }
        answer_exact_review(cx, ui, review, matches!(action, TaskPermissionAction::Allow(_)));
        ui.redraw(cx);
        return;
    }
    ui.modal(cx, ids!(task_permission_modal)).close(cx);
    let Some(Some(prompt)) = with_a2app(|state| state.active_task.take()) else { return };
    if matches!(action, TaskPermissionAction::None) {
        with_a2app(|state| state.active_task = Some(prompt));
        return;
    }
    let TaskPrompt { room_id, plan, resume } = prompt;
    let approved = with_a2app(|state| {
        task_answer_approval(&room_id, &plan, &action, &mut state.dismissed_task_plans)
    })
    .unwrap_or_default();
    // A host-raised carried-over prompt is not the agent's tool call, so it
    // leaves no `ai_tool_call` row of its own.
    let agent_tool = matches!(&resume, TaskResume::AgentTool(_));
    // Apply outside the state borrow: information-flow rules live in their own
    // registry, and a partial apply is rolled back by `task_grants::apply`.
    let mut guard = PermissionStoreGuard::take();
    let result = task_grants::apply(&plan, &approved, guard.store(), &task_grants::GlobalFlowApply);
    let mut applied = None;
    let outcome = match result {
        Ok(task) => {
            // Report the states re-checked at apply time, then keep the task in
            // the ledger for the turn's close to revoke.
            let outcome = task_grants::outcome_of(&plan, &approved, &task);
            applied = Some(task);
            if agent_tool {
                let fully_allowed = outcome["status"] == "granted";
                let note = if fully_allowed { format!("Approved: {}", plan.title) }
                    else if outcome["status"] == "declined" { "Declined".to_string() }
                    else { format!("Some needs were refused: {}", plan.title) };
                note_ai_tool_call(&room_id, "request_task_permissions", fully_allowed, &note);
            }
            Ok(outcome.to_string())
        }
        Err(error) => {
            let message = error.message();
            if agent_tool {
                note_ai_tool_call(&room_id, "request_task_permissions", false, &message);
            }
            Err(message)
        }
    };
    guard.restore(|state| {
        if let Some(task) = applied {
            state.task_ledger.insert(task);
        }
    });
    publish_grants(cx);
    match resume {
        TaskResume::AgentTool(answer) => {
            let _ = answer.send(outcome);
        }
        TaskResume::UserPrompt { texts } => {
            if let Err(error) = outcome {
                // The carried-over rules could not be applied; the turn still
                // runs, so say why before the model call is refused.
                log!("AI Rooms: couldn't apply the carried-over permissions for {room_id}: {error}");
            }
            // Deliver with the carried check disabled: this batch has been
            // answered, so it must not raise the same prompt again.
            forward_ai_room_texts_inner(&room_id, texts, false);
        }
    }
    show_next_permission_prompt(cx, ui);
    ui.redraw(cx);
}

/// Applies the user's answer to one exact-action review. Allow grants the
/// captured once-authority and resumes the effect; Not now refuses it. If the
/// influence set grew while the modal was open, the old approval no longer
/// matches and a fresh review is raised instead of failing forever.
#[cfg(unix)]
fn answer_exact_review(cx: &mut Cx, ui: &WidgetRef, review: ExactReviewPrompt, approved: bool) {
    let ExactReviewPrompt { room_id, context, epoch, action, payload, expected, request_id, resume, .. } = review;
    if !approved {
        let _ = a2app_core::information_flow::cancel_exact_action(request_id);
        refuse_exact_resume(resume,
            String::from("The user did not allow this action. Do not retry it; tell the user what you could not do."));
        show_next_permission_prompt(cx, ui);
        return;
    }
    if a2app_core::information_flow::grant_exact_action_for_activation(&context, request_id, &expected, epoch).is_ok() {
        resume_exact(cx, ui, room_id, resume);
        show_next_permission_prompt(cx, ui);
        return;
    }
    // Influence grew between the review and the answer. Capture a fresh
    // request for the current label and show it again.
    let _ = a2app_core::information_flow::cancel_exact_action(request_id);
    match a2app_core::information_flow::check_exact_action_for_activation(&context, epoch, &action, &payload) {
        Ok(()) => {
            resume_exact(cx, ui, room_id, resume);
            show_next_permission_prompt(cx, ui);
        }
        Err(error) => {
            if let Some((new_request, new_expected)) = pending_exact_decision(&context, epoch, &action) {
                let info = exact_review_info(&action, &payload);
                with_a2app(|state| state.exact_reviews.push_back(ExactReviewPrompt {
                    room_id, context, epoch, action, payload, expected: new_expected,
                    request_id: new_request, info, resume,
                }));
                show_next_permission_prompt(cx, ui);
            } else {
                refuse_exact_resume(resume, error);
                show_next_permission_prompt(cx, ui);
            }
        }
    }
}

/// The items one task answer actually approves. `Not now` approves nothing and
/// is remembered as a dismissal so the same needs are refused for the turn;
/// `Allow` narrows the checked ids so an unchecked read drops the flow rules
/// that depend on it. `None` never reaches a real answer.
#[cfg(unix)]
fn task_answer_approval(
    room_id: &OwnedRoomId,
    plan: &TaskPlan,
    action: &TaskPermissionAction,
    dismissed: &mut HashMap<(String, [u8; 32]), BTreeSet<String>>,
) -> BTreeSet<String> {
    match action {
        TaskPermissionAction::NotNow => {
            dismissed.insert((room_id.to_string(), plan.needs_fingerprint), plan.need_keys());
            BTreeSet::new()
        }
        TaskPermissionAction::Allow(ids) => task_grants::dependent_approval(plan, ids.clone()),
        TaskPermissionAction::None => BTreeSet::new(),
    }
}

/// Revokes every task grant a room applied and forgets its "Not now" memory.
/// Also refuses any exact-action review parked for the room, so a serve
/// thread waiting on one never hangs across a turn or teardown.
/// Called when the turn closes, when the session stops, and on room close.
#[cfg(unix)]
fn revoke_task_grants(room_id: &OwnedRoomId) {
    let reviews: Vec<ExactReviewPrompt> = with_a2app(|state| {
        let (mine, rest): (Vec<_>, Vec<_>) = state.exact_reviews.drain(..).partition(|review| &review.room_id == room_id);
        state.exact_reviews = rest.into_iter().collect();
        let mut pending = mine;
        if state.active_exact_review.as_ref().is_some_and(|review| &review.room_id == room_id) {
            if let Some(review) = state.active_exact_review.take() {
                pending.push(review);
            }
        }
        pending
    })
    .unwrap_or_default();
    for review in reviews {
        let _ = a2app_core::information_flow::cancel_exact_action(review.request_id);
        refuse_exact_resume(review.resume,
            String::from("The turn ended before this action was reviewed. Retry it if it is still needed."));
    }
    let tasks: Vec<task_grants::AppliedTask> = with_a2app(|state| {
        state.dismissed_task_plans.retain(|(room, _), _| room != room_id.as_str());
        state.task_request_counts.remove(room_id.as_str());
        // Grants are gone, so no final write still needs them; forget any
        // outstanding write bookkeeping so a later turn's tokens are clean.
        if let Some(info) = state.ai_rooms.get_mut(room_id) {
            info.pending_final_writes.clear();
            info.final_writes_pending_revoke = false;
            info.pending_final_turns.clear();
            info.final_writes_deadline = None;
        }
        let ids: Vec<u64> = state
            .task_ledger
            .tasks()
            .filter(|task| task.context.room() == Some(room_id.as_str()))
            .map(|task| task.task_id)
            .collect();
        ids.into_iter().filter_map(|id| state.task_ledger.remove(id)).collect()
    })
    .unwrap_or_default();
    if tasks.is_empty() {
        return;
    }
    let mut guard = PermissionStoreGuard::take();
    let mut revoked_cleanly = true;
    for task in &tasks {
        revoked_cleanly &= task_grants::rollback(task, guard.store(), &task_grants::GlobalFlowApply);
    }
    guard.restore(|_| {});
    publish_current_grants();
    if !revoked_cleanly {
        enqueue_popup_notification(
            "Some of this turn's sharing rules could not be revoked. Review Data Sharing to be sure no access was left behind.",
            PopupKind::Warning, Some(8.0),
        );
    }
}

/// Takes every task prompt pending for a room (without a UI handle), so a
/// teardown path can answer its caller even when it cannot close the modal.
///
/// A host-raised carried-over prompt ([`TaskResume::UserPrompt`]) holds the
/// user messages it was raised for; dropping it here loses those copies, but
/// the room's cursor was never advanced for a held message, so the next
/// timeline update re-forwards them and re-raises the prompt.
#[cfg(unix)]
fn take_room_tasks(room_id: &OwnedRoomId) -> Vec<TaskPrompt> {
    with_a2app(|state| {
        let mut pending = Vec::new();
        if state.active_task.as_ref().is_some_and(|prompt| &prompt.room_id == room_id)
            && let Some(prompt) = state.active_task.take()
        {
            pending.push(prompt);
        }
        let (mine, rest): (Vec<_>, Vec<_>) = state
            .task_prompts
            .drain(..)
            .partition(|prompt| &prompt.room_id == room_id);
        state.task_prompts = rest.into_iter().collect();
        pending.extend(mine);
        pending
    })
    .unwrap_or_default()
}

/// Withdraws a room's pending task prompts (turn cancelled or session gone):
/// the parked tool calls are refused and the modal moves on, so a serve thread
/// never hangs.
#[cfg(unix)]
fn refuse_room_tasks(cx: &mut Cx, ui: &WidgetRef, room_id: &OwnedRoomId) {
    let (pending, was_active) = with_a2app(|state| {
        let mut pending = Vec::new();
        let mut was_active = false;
        if state.active_task.as_ref().is_some_and(|prompt| &prompt.room_id == room_id) {
            if let Some(prompt) = state.active_task.take() {
                pending.push(prompt);
                was_active = true;
            }
        }
        let (mine, rest): (Vec<_>, Vec<_>) = state
            .task_prompts
            .drain(..)
            .partition(|prompt| &prompt.room_id == room_id);
        state.task_prompts = rest.into_iter().collect();
        pending.extend(mine);
        (pending, was_active)
    })
    .unwrap_or_default();
    for prompt in pending {
        if let TaskResume::AgentTool(answer) = prompt.resume {
            let message = task_grants::outcome(&prompt.plan, &std::collections::BTreeSet::new()).to_string();
            let _ = answer.send(Ok(message));
        }
    }
    let (reviews, review_was_active) = with_a2app(|state| {
        let (mine, rest): (Vec<_>, Vec<_>) = state.exact_reviews.drain(..).partition(|review| &review.room_id == room_id);
        state.exact_reviews = rest.into_iter().collect();
        let mut pending = mine;
        let mut was_active = false;
        if state.active_exact_review.as_ref().is_some_and(|review| &review.room_id == room_id) {
            if let Some(review) = state.active_exact_review.take() {
                pending.push(review);
                was_active = true;
            }
        }
        (pending, was_active)
    })
    .unwrap_or_default();
    for review in reviews {
        let _ = a2app_core::information_flow::cancel_exact_action(review.request_id);
        refuse_exact_resume(review.resume, String::from("The request was cancelled before this action was reviewed."));
    }
    if was_active || review_was_active {
        ui.modal(cx, ids!(task_permission_modal)).close(cx);
        show_next_permission_prompt(cx, ui);
    }
}

/// Pushes a changed grant into the app's live isolate: network changes
/// stop the app (the net runtime is baked in at VM alloc); anything else
/// just gets the new caps list plus an `on_permissions_changed` call.
fn apply_permission_to_running(cx: &mut Cx, _ui: &WidgetRef, app_id: &str, perm: Permission) {
    // The mcp-tools answer is a kill switch, so a Deny takes back the tools
    // the app already installed on a room's agent, not just future ones.
    #[cfg(unix)]
    if perm == Permission::McpTools {
        withdraw_app_tools_if_denied(app_id);
    }
    prune_hook_subs();
    if !with_a2app(|state| state.is_running(app_id)).unwrap_or(false) {
        return;
    }
    let grants = a2app_core::permissions::snapshot_grants_for(app_id);
    instances::update_app_caps(cx, app_id, grants);
}

/// Whether `app_id` may offer tools to a room's agent right now: the same
/// capability decision every other service gets, so an App Info Deny (group
/// or the `mcp.tools.register` row under it) counts.
#[cfg(unix)]
fn app_tools_allowed(app_id: &str) -> bool {
    let Some(cap) = a2app_core::capabilities::by_id("mcp.tools.register") else { return false };
    with_a2app(|state| {
        let Some(manifest) = state.registry.get(app_id) else { return false };
        !matches!(
            state.permissions.effective_capability(manifest, cap),
            Effective::Denied | Effective::Undeclared
        )
    })
    .unwrap_or(false)
}

/// Removes every tool `app_id` registered, off each room's live session, and
/// answers any model call parked on one. Used when the user withdraws the
/// app's permission to offer them.
#[cfg(unix)]
fn withdraw_app_tools(app_id: &str) {
    let mut failed_rooms = Vec::new();
    let withdrawn = with_a2app(|state| {
        let names: Vec<String> = state
            .app_tools
            .iter()
            .filter(|(_, reg)| reg.app_id == app_id)
            .map(|(name, _)| name.clone())
            .collect();
        for name in &names {
            if let Some(reg) = state.app_tools.remove(name) {
                if transfer_app_tool_provenance(state, &reg.app_id, reg.heap_key, &reg.room_id).is_err() {
                    state.ai_sessions.remove(&reg.room_id);
                    failed_rooms.push(reg.room_id.clone());
                }
                if let Some(session) = state.ai_sessions.get(&reg.room_id) {
                    session.unregister_miniapp_tool(name);
                }
            }
        }
        let ids: Vec<u64> = state
            .app_tool_calls
            .iter()
            .filter(|(_, p)| names.contains(&p.full_name))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(p) = state.app_tool_calls.remove(&id) {
                let _ = p.answer.send(Err(String::from(
                    "this mini-app is no longer allowed to offer tools",
                )));
            }
        }
        names
    })
    .unwrap_or_default();
    for room in failed_rooms { stop_ai_session(&room); }
    if withdrawn.is_empty() {
        return;
    }
    log!("a2app: withdrew {} AI tool(s) from app {app_id}: {withdrawn:?}", withdrawn.len());
    enqueue_popup_notification(
        format!(
            "Took back {} AI tool{} from this app.",
            withdrawn.len(),
            if withdrawn.len() == 1 { "" } else { "s" }
        ),
        PopupKind::Info,
        Some(4.0),
    );
}

/// Withdraws an app's AI tools if it may no longer offer them.
#[cfg(unix)]
fn withdraw_app_tools_if_denied(app_id: &str) {
    if !app_tools_allowed(app_id) {
        withdraw_app_tools(app_id);
    }
}

/// Restarts a running app's isolate on whichever surface hosts it, so the
/// current source and grants take effect (the net runtime especially, which
/// is baked in at VM alloc).
/// Quits the app wherever it runs so its next open boots the new source or
/// grants. Returns whether anything was running. Swapping the isolate under
/// a live pane leaves the old frame on screen and a pane rebuilt straight
/// after its teardown takes no input, so the user reopens it instead.
fn stop_for_restart(cx: &mut Cx, ui: &WidgetRef, manifest: &MiniAppManifest) -> bool {
    let was_running = instances::is_running(&manifest.id);
    if was_running {
        stop_app_everywhere(cx, ui, &manifest.id);
    }
    prune_hook_subs();
    was_running
}

/// Tacks the reopen hint onto a popup when the app had to be stopped.
fn reopen_hint(done: String, was_running: bool) -> String {
    if was_running { format!("{done} It was stopped; open it again to run this version.") } else { done }
}

fn expire_timed_grants(cx: &mut Cx, ui: &WidgetRef) {
    let expired = with_a2app(|state| {
        let now = Instant::now();
        if now.duration_since(state.last_timed_check) < TIMED_GRANT_CHECK {
            return Vec::new();
        }
        state.last_timed_check = now;
        let expired = state.permissions.expire_timed(a2app_core::versions::now_unix());
        if !expired.is_empty() {
            state.perms_dirty = true;
        }
        expired
    }).unwrap_or_default();
    if expired.is_empty() {
        return;
    }
    publish_grants(cx);
    for (app_id, perm) in expired {
        apply_permission_to_running(cx, ui, &app_id, perm);
    }
}

fn invalidate_policy_spaces(cx: &mut Cx) {
    with_a2app(|state| {
        state.permissions.clear_room_spaces();
        state.policy_spaces_last = None;
        state.policy_spaces_ready = false;
        state.policy_spaces_revision = matrix::spaces::policy_spaces_revision();
    });
    publish_grants(cx);
}

fn request_policy_spaces() {
    if crate::sliding_sync::get_client().is_none() { return; }
    let refresh = with_a2app(|state| {
        if state.policy_spaces_ready || state.policy_spaces_pending || state.policy_spaces_last.is_some_and(|last| last.elapsed() < Duration::from_secs(60)) {
            return false;
        }
        state.policy_spaces_pending = true;
        state.policy_spaces_status = Some("Refreshing space membership; rooms with unresolved protection remain blocked.".into());
        state.policy_spaces_last = Some(Instant::now());
        true
    }).unwrap_or(false);
    if refresh {
        submit_async_request(MatrixRequest::A2App(matrix::A2AppMatrixRequest::RefreshPolicySpaces));
    }
}

/// Ends room-session consent when the room's last tab/mobile screen closes.
pub fn on_room_closed(cx: &mut Cx, room_id: &OwnedRoomId) {
    // Task grants are turn-scoped and never outlive the room session.
    #[cfg(unix)]
    revoke_task_grants(room_id);
    if let Ok(account) = super::information_flow::account() {
        let _ = a2app_core::information_flow::close_room_session(&account, room_id.as_str());
    }
    with_a2app(|state| { state.permissions.clear_room_session(room_id.as_str()); });
    publish_grants(cx);
    cx.action(A2AppOp::RoomClosed(room_id.clone()));
}

/// Context-specific capabilities advertised to one isolate.
pub fn grants_in_room(app_id: &str, room: Option<&str>) -> Vec<String> {
    with_a2app(|state| {
        let Some(manifest) = state.registry.get(app_id) else { return Vec::new() };
        let context = PermissionContext { origin_room: room, target_room: room };
        let mut grants: Vec<String> = Permission::ALL.into_iter().filter(|permission| {
            state.permissions.effective_for_in_context(app_id, |p| manifest.declares(p), *permission, context) == Effective::Granted
        }).map(|permission| permission.as_str().to_string()).collect();
        for cap in a2app_core::capabilities::CATALOG.iter() {
            let effective = if services::is_room_collection(cap) {
                state.permissions.effective_collection_capability_in_context(manifest, cap, context)
            } else { state.permissions.effective_capability_in_context(manifest, cap, context) };
            if effective == Effective::Granted {
                grants.push(cap.id.to_string());
                if let Some(group) = cap.group { grants.push(group.as_str().to_string()); }
            }
        }
        grants.sort();
        grants.dedup();
        grants
    }).unwrap_or_default()
}

/// Hard room protections apply even to automatic agent input/output.
fn room_policy_allows(room: &str, access: RoomAccess) -> bool {
    with_a2app(|state| state.permissions.room_policy(Some(room), access) != PolicyDecision::Deny)
        .unwrap_or(false)
}

fn retire_flow_context(cx: &mut Cx, ui: &WidgetRef, context: &a2app_core::information_flow::ContextId) {
    if !super::information_flow::account().is_ok_and(|account| context.account() == account) { return; }
    match context {
        a2app_core::information_flow::ContextId::Agent { room, .. } =>
            permission_lifecycle::retire_after_revocation(cx, ui, &agent_subject(room)),
        _ => if let Some(app) = context.app() { permission_lifecycle::retire_after_revocation(cx, ui, app); },
    }
}

fn stop_private_contexts(cx: &mut Cx, ui: &WidgetRef) {
    with_a2app(|state| { state.generation = None; state.generation_context = None; state.generation_epoch = None; state.pending_generated = None; state.console.review_context = None; });
    #[cfg(unix)]
    {
        let rooms = with_a2app(|state| state.ai_sessions.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
        for room in rooms {
            abort_ai_room_work(cx, ui, &room);
        }
    }
    let apps = with_a2app(|state| state.registry.iter().map(|m| m.id.clone()).collect::<Vec<_>>()).unwrap_or_default();
    for app in apps { stop_app_everywhere(cx, ui, &app); }
}

fn reset_all_permissions(cx: &mut Cx, ui: &WidgetRef) {
    let Some((prompts, cancel_generation)) = with_a2app(|state| {
        let mut prompts = state.prompts.drain(..).collect::<Vec<_>>();
        prompts.extend(state.active_prompt.take());
        prompts.append(&mut state.active_prompt_batch);
        state.permissions.reset_to_defaults();
        state.perms_dirty = true;
        state.dismissed_prompts.clear();
        state.dismissed_net_hosts.clear();
        state.dismissed_effects.clear();
        state.permission_gestures.clear();
        state.room_action = None;
        #[cfg(unix)]
        state.once_rooms.clear();
        (prompts, state.generation.is_some() || state.pending_generated.is_some())
    }) else { return };
    // Publish revocation before waking blocked callers or retiring their VMs.
    publish_grants(cx);
    ui.modal(cx, ids!(a2app_permission_modal)).close(cx);
    for prompt in prompts { permission_batch::refuse_prompt(cx, prompt, "Permissions were reset."); }
    if cancel_generation {
        // Use ordinary cancellation to answer the agent's waiting build tool
        // and update its console before private contexts are torn down.
        apply_op(cx, ui, A2AppOp::CancelGeneration);
        with_a2app(|state| {
            state.generation_attribution = None;
            state.console.status = "Cancelled because permissions were reset.".into();
        });
    }
    // Background-task enablement is consent too. Keep the tasks and their
    // configuration, but require the user to enable them again after reset.
    let background_result = super::background::reset_permissions(cx);
    stop_private_contexts(cx, ui);
    super::room_watch::stop_all();
    with_a2app(|state| {
        state.hook_subs.clear();
        state.watched_rooms.clear();
        state.account_watched = false;
        state.foreground_app = None;
    });
    ui.modal(cx, ids!(mini_app_host_modal)).close(cx);
    let flow_result = a2app_core::information_flow::reset_permissions();
    publish_grants(cx);
    // Save immediately: closing Robrix just after reset must not restore the
    // old approvals. Leave a failed save dirty so normal persistence retries.
    let save_result = with_a2app(|state| {
        let result = persistence::save_permissions(&state.permissions);
        state.perms_dirty = result.is_err();
        result.map_err(|error| error.to_string())
    }).unwrap_or_else(|| Err("Mini Apps is unavailable.".into()));
    if let Ok(account) = super::information_flow::account() {
        a2app_core::protection_audit::record_policy_change(&account);
    }
    match flow_result.and(background_result).and(save_result) {
        Ok(()) => enqueue_popup_notification("All permissions reset. Apps and agents will ask again; background tasks are paused.", PopupKind::Success, Some(5.0)),
        Err(error) => enqueue_popup_notification(format!("Couldn't save the permissions reset: {error}"), PopupKind::Error, Some(8.0)),
    }
    ui.redraw(cx);
}

fn refresh_permission_policy(cx: &mut Cx, ui: &WidgetRef) {
    if let Ok(account) = super::information_flow::account() {
        a2app_core::protection_audit::record_policy_change(&account);
    }
    publish_grants(cx);
    prune_hook_subs();
    let apps = with_a2app(|state| state.registry.iter().map(|m| m.id.clone()).collect::<Vec<_>>()).unwrap_or_default();
    for app in apps {
        // Restart affected app state after policy changes. Host requests also
        // recheck live permissions, including requests already queued.
        if instances::is_running(&app) {
            stop_app_everywhere(cx, ui, &app);
        }
    }
    #[cfg(unix)]
    {
        let blocked = with_a2app(|state| state.ai_sessions.keys()
            .filter(|room| state.permissions.room_policy(Some(room.as_str()), RoomAccess::Read) == PolicyDecision::Deny)
            .cloned().collect::<Vec<_>>()).unwrap_or_default();
        for room in blocked {
            abort_ai_room_work(cx, ui, &room);
        }
    }
    ui.redraw(cx);
}

/// Republishes the grant snapshot that isolate-creation sites read.
fn publish_grants(_cx: &mut Cx) {
    publish_current_grants();
}

/// Publishes permission changes at the worker boundary, including rollbacks
/// that finish without a UI event handler or a `Cx`.
fn publish_current_grants() {
    super::background::changed();
    let account = super::information_flow::account().ok();
    let effect_spaces: std::collections::BTreeSet<String> = a2app_core::information_flow::effect_authorities().unwrap_or_default()
        .into_iter().filter(|grant| Some(grant.context.account()) == account.as_deref())
        .filter_map(|grant| grant.room_scope)
        .flat_map(|scope| match scope { RoomScope::Selection { spaces, .. } => spaces, RoomScope::AllRooms => Vec::new() }).collect();
    matrix::policy::publish_effect_space_roots(effect_spaces.clone());
    with_a2app(|state| {
        let mut roots = state.permissions.configured_space_ids();
        roots.extend(effect_spaces);
        if roots != state.policy_space_roots {
            // A complete joined-space graph already covers these roots. Keep
            // it usable for the request resuming immediately after approval.
            let client = crate::sliding_sync::get_client();
            let covered = state.policy_spaces_ready
                && state.policy_spaces_revision == matrix::spaces::policy_spaces_revision()
                && roots.difference(&state.policy_space_roots).all(|space|
                    state.permissions.room_space_memberships().contains_key(space)
                        && RoomId::parse(space).ok().and_then(|id| client.as_ref()?.get_room(&id))
                            .is_some_and(|room| room.is_space() && room.state() == RoomState::Joined));
            state.policy_space_roots = roots;
            if !covered { matrix::spaces::invalidate_policy_spaces(); }
        }
        let revision = matrix::spaces::policy_spaces_revision();
        if state.policy_spaces_revision != revision {
            state.permissions.clear_room_spaces();
            state.policy_spaces_ready = false;
            state.policy_spaces_last = None;
            state.policy_spaces_revision = revision;
        }
        a2app_core::permissions::publish_snapshot(state.permissions.snapshot(&state.registry));
        crate::a2app::matrix::publish_permission_policy_at_revision(&state.permissions, state.policy_spaces_revision);
    });
}


/// The local UTC offset, for version-history timestamps in local time.
pub fn utc_offset_secs() -> i64 {
    chrono::Local::now().offset().local_minus_utc() as i64
}

fn persist_if_dirty() {
    with_a2app(|state| {
        let now = Instant::now();
        if now.duration_since(state.last_persist) < PERSIST_THROTTLE {
            return;
        }
        save_dirty(state);
        state.last_persist = now;
    });
}

/// Saves any dirty state immediately; called on app shutdown/pause.
pub fn persist_now() {
    with_a2app(save_dirty);
}

fn save_dirty(state: &mut A2AppState) {
    if state.perms_dirty {
        match persistence::save_permissions(&state.permissions) {
            Ok(()) => state.perms_dirty = false,
            Err(e) => error!("Failed to save mini-app permissions: {e}"),
        }
    }
    if state.registry_dirty {
        if let Err(e) = persistence::save_registry_state(&state.persisted) {
            error!("Failed to save mini-app registry state: {e}");
        }
        state.registry_dirty = false;
    }
}

/// How long shutdown waits for a cancelled agent to end its turn before its
/// process is killed anyway.
const AI_SHUTDOWN_GRACE: Duration = Duration::from_millis(2000);

/// Aborts every in-flight AI operation as the app shuts down: the one running
/// generation (if any) and each live room session's current turn.
///
/// Each agent is told to abandon its turn with an explicit `session/cancel` —
/// not just killed — so it can stop the LLM-provider request it is waiting
/// on, and then a short grace is spent draining the sessions until their
/// cancelled turns actually end (a `TurnDone` with a `cancelled` stop reason)
/// or the grace expires. Whatever is still busy when this returns is cut off
/// by process death: the held [`Generation`] drops here (killing its agent
/// child), and the sessions' transports drop when the thread-local state is
/// torn down right after. Provider-side work is therefore stopped two ways —
/// the explicit cancel while the agent is alive to relay it, and the dropped
/// HTTP connection once the child dies (streaming providers abort on that).
///
/// Called from `App::handle_shutdown` after state persistence. Returns
/// immediately when nothing is running.
#[cfg(unix)]
pub fn shutdown() {
    matrix::spaces::stop_policy_space_watch();
    // Take the generation out of state and cancel its agent. It is held here
    // (rather than dropped) for the grace below: dropping kills the child
    // immediately, which can race the cancel that was just written to it.
    let mut generation = with_a2app(|state| state.generation.take()).flatten();
    if let Some(generation) = generation.as_mut() {
        generation.cancel();
    }
    let had_work = with_a2app(|state| {
        let mut had = generation.is_some();
        for session in state.ai_sessions.values_mut() {
            had |= session.cancel_for_shutdown();
        }
        had
    })
    .unwrap_or(false);
    if !had_work {
        return;
    }
    // Let the cancels land: drain each session until its turn is over. A live
    // generation's cancellation isn't observable here (driving it needs a
    // `Cx`, which shutdown no longer has), so it gets the full bounded grace
    // before its child is killed on drop below.
    let deadline = Instant::now() + AI_SHUTDOWN_GRACE;
    while Instant::now() < deadline {
        let sessions_idle = with_a2app(|state| {
            for session in state.ai_sessions.values_mut() {
                session.advance();
            }
            state.ai_sessions.values().all(|s| !s.is_busy())
        })
        .unwrap_or(true);
        if sessions_idle && generation.is_none() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Dropping the generation here kills its agent child; the sessions'
    // transports die with the process teardown right after this returns.
    drop(generation);
}

/// Runs a `/miniapp` slash command typed in a room: bare opens the Mini Apps
/// screen, "run <name>" opens an installed app attached to this room,
/// "share <name>" posts an installed app's bundle into the room, and
/// anything else starts a room-scoped generation.
pub fn run_miniapp_command(cx: &mut Cx, arg: &str, room_id: &OwnedRoomId) {
    let arg = arg.trim();
    if arg.is_empty() {
        cx.action(crate::home::navigation_tab_bar::NavigationBarAction::GoToMiniApps);
        return;
    }
    // "run <name>" and "share <name>" both address an installed app.
    let find_app = |name: &str| with_a2app(|state| {
        state.registry.iter()
            .find(|a| a.name.eq_ignore_ascii_case(name) || a.id.eq_ignore_ascii_case(name))
            .map(|a| a.id.clone())
    }).flatten();
    let no_such_app = |name: &str| enqueue_popup_notification(
        format!("No mini-app named \"{name}\". Check the Mini Apps screen for the exact name."),
        PopupKind::Error, Some(5.0),
    );

    if let Some(name) = arg.strip_prefix("run ") {
        let name = name.trim();
        match find_app(name) {
            // The whole point of this form: the app opens ATTACHED to this
            // room, docked into this RoomScreen's own pane.
            Some(app_id) => cx.action(A2AppOp::OpenApp {
                app_id,
                room_id: Some(room_id.clone()),
                in_room_pane: true,
            }),
            None => no_such_app(name),
        }
        return;
    }
    if let Some(name) = arg.strip_prefix("share ") {
        let name = name.trim();
        match find_app(name) {
            Some(app_id) => cx.action(A2AppOp::ShareToRoom { app_id, room_id: room_id.clone() }),
            None => no_such_app(name),
        }
        return;
    }
    // Show the console while the room-scoped generation runs.
    cx.action(crate::home::navigation_tab_bar::NavigationBarAction::GoToMiniApps);
    cx.action(A2AppOp::StartGeneration {
        request: arg.to_string(),
        room_id: Some(room_id.clone()),
    });
}

// -----------------------------------------------------------------------
// AI Rooms
// -----------------------------------------------------------------------

/// Called when a `RoomScreen` first opens a room: checks (once) whether it
/// carries the `rs.robius.robrix.ai_room` marker, so its session can be
/// attached. Cheap to call on every room open — a room already known either
/// way is a synchronous map lookup, no request goes out.
#[cfg(unix)]
pub fn on_room_shown(room_id: &OwnedRoomId) {
    let already_known = with_a2app(|state| {
        state.ai_rooms.contains_key(room_id) || state.known_non_ai_rooms.contains(room_id)
    }).unwrap_or(true);
    if already_known {
        log!("AI Rooms: room {room_id} already known (ai_room? {}); no re-check.", with_a2app(|s| s.ai_rooms.contains_key(room_id)).unwrap_or(false));
        return;
    }
    log!("AI Rooms: room {room_id} shown for the first time; checking for the ai_room marker...");
    submit_async_request(MatrixRequest::AiRoom(AiRoomRequest::CheckMarker {
        room_id: room_id.clone(),
    }));
}

/// Applies one Matrix-worker result for an AI-room operation. Room creation
/// (`Created`/`CreateFailed`) is navigation, so `app.rs` handles it instead.
#[cfg(unix)]
fn apply_ai_room_action(cx: &mut Cx, ui: &WidgetRef, action: AiRoomAction) {
    match action {
        AiRoomAction::Created { room_name_id } => {
            // The room was just created with the ai_room marker in its
            // initial_state, so record it as an AI room right away. Its
            // marker state event can lag sliding sync by a second or two
            // after creation; recording it here means the room is already
            // recognized (session attached on its first forwarded message)
            // without waiting for that sync to land.
            let room_id = room_name_id.room_id().clone();
            log!("AI Rooms: recording newly-created AI room {room_id} ({}); no marker check needed.", room_name_id.display());
            with_a2app(|state| {
                state.ai_rooms.entry(room_id).or_insert_with(|| AiRoomInfo::new(None));
            });
        }
        AiRoomAction::CreateFailed { error } => {
            // app.rs already showed the error popup.
            log!("AI Rooms: AI room creation failed: {error}");
        }
        AiRoomAction::MarkFailed { error } => {
            log!("AI Rooms: FAILED to mark the room as an AI room: {error}");
            enqueue_popup_notification(
                format!("Couldn't mark this room as an AI room: {error}"),
                PopupKind::Error, Some(6.0),
            );
        }
        AiRoomAction::NotAiRoom { room_id } => {
            log!("AI Rooms: room {room_id} has no ai_room marker; treating it as an ordinary room.");
            with_a2app(|state| { state.known_non_ai_rooms.insert(room_id); });
        }
        AiRoomAction::Attached { room_id, name, cursor } => {
            log!("AI Rooms: room {room_id} is an AI room (name: {name:?}, saved forwarding cursor: {cursor:?}); attaching session.");
            // A second marker check for a room already attached (the room
            // was shown twice before the first answer landed) must not reset
            // its live cursor and turn state.
            let already_known = with_a2app(|state| {
                if state.ai_rooms.contains_key(&room_id) {
                    return true;
                }
                state.ai_rooms.insert(room_id.clone(), AiRoomInfo::new(cursor));
                false
            })
            .unwrap_or(true);
            if already_known {
                log!("AI Rooms: room {room_id} is already attached; ignoring the duplicate.");
            } else {
                attach_ai_session(cx, ui, &room_id, name);
            }
        }
        AiRoomAction::PostReplyResult { room_id, answer_id, write_id, result } => {
            let parked = answer_id
                .and_then(|id| with_a2app(|state| state.ai_replies.remove(&id)).flatten());
            if let Err(error) = &result {
                log!("AI Rooms: FAILED to post an ai_reply state event: {error}");
                enqueue_popup_notification(
                    format!("Couldn't post the AI agent's reply: {error}"),
                    PopupKind::Error, Some(6.0),
                );
            }
            // A natural reply is the closed turn's final output. Its result
            // releases the turn's task grants once every other final write has
            // landed; a `send_message` reply belongs to a still-live turn and
            // never releases them. The next member turn waits until every
            // final write has landed and the closed turn's grants are revoked.
            if answer_id.is_none() {
                final_write_landed(&room_id, write_id);
            }
            let Some((_, turn, answer, receipts)) = parked else { return };
            // The write outlives a turn the user cancelled (its tool call is
            // abandoned, not awaited), so a result whose turn is over may only
            // answer the call — never touch the turn that took its place.
            let live_turn = with_a2app(|state| {
                state
                    .ai_rooms
                    .get(&room_id)
                    .and_then(|info| info.active_turn.as_ref().map(|t| t.key.clone()))
            })
            .flatten();
            let same_turn = live_turn == turn;
            match result {
                Ok(()) => {
                    // Only a landed write may silence the turn's trailing
                    // text: the tool really did say this turn's piece.
                    if same_turn {
                        with_a2app(|state| {
                            if let Some(info) = state.ai_rooms.get_mut(&room_id) {
                                info.posted_by_tool_this_turn = true;
                            }
                        });
                    }
                    let _ = answer.send(Ok(String::from("Posted to the room.")));
                }
                Err(error) => {
                    // Nothing was posted: give the turn its receipts back (minus
                    // this call's optimistic one, re-added as failed below) so
                    // they ride the reply the turn will now post itself.
                    if same_turn {
                        with_a2app(|state| {
                            if let Some(info) = state.ai_rooms.get_mut(&room_id) {
                                let mut restored = receipts;
                                if restored.last().is_some_and(|c| c.name == "send_message") {
                                    restored.pop();
                                }
                                restored.append(&mut info.pending_tool_calls);
                                info.pending_tool_calls = restored;
                            }
                        });
                        note_ai_tool_call(&room_id, "send_message", false, &error);
                    }
                    let _ = answer.send(Err(error));
                }
            }
        }
        AiRoomAction::PostToRoomResult { id, result } => {
            // A cross-room post finished on the worker; answer the tool call
            // that has been waiting on it and record the receipt (and the
            // call's live state row) with its outcome. The sender is gone if
            // the session ended meanwhile — a send failure is the cleanup.
            if let Some((session_room, answer)) =
                with_a2app(|state| state.ai_posts.remove(&id)).flatten()
            {
                let (summary, ok) = match &result {
                    Ok(msg) => (msg.clone(), true),
                    Err(e) => (e.clone(), false),
                };
                note_ai_tool_call(&session_room, "post_room_message", ok, &summary);
                let _ = answer.send(result);
            }
        }
        AiRoomAction::ToolReadResult { id, result, authorization } => {
            // A read finished on the worker; answer the tool call that has
            // been waiting on it. A granted read's outcome is recorded on the
            // turn's receipt; ungated memory reads (recalling its own past
            // turns) are room plumbing, not something the user watches for,
            // so they leave no chip. The sender is gone if the session ended
            // meanwhile — a send failure is the cleanup (and no receipt is
            // left behind).
            if let Some((room_id, kind, answer)) =
                with_a2app(|state| state.ai_reads.remove(&id)).flatten()
            {
                let result = result.and_then(|result| with_a2app(|state| {
                    let auth = authorization;
                    let target = match &kind {
                        ReadToolKind::OtherRoom { room, .. } => room.as_str(),
                        ReadToolKind::SpaceInfo { space } | ReadToolKind::SpaceRooms { space } => space.as_str(),
                        _ => room_id.as_str(),
                    };
                    if state.permissions.room_policy(Some(target), RoomAccess::Read) == PolicyDecision::Deny
                        || auth.as_ref().is_some_and(|auth| !auth.permits_request(&state.permissions))
                        || (kind.capability().is_some() && auth.is_none())
                    { return Err("Reading this room is now blocked in Mini Apps permissions.".into()); }
                    matrix::policy::filter_read_result(&result, &state.permissions, auth.as_ref())
                }).unwrap_or_else(|| Err("Permissions are unavailable.".into())));
                let result = result.and_then(|text| {
                    let context = super::information_flow::agent_context(room_id.as_str())?;
                    if is_directory_kind(&kind) {
                        super::information_flow::record_directory_response(&context)?;
                    } else {
                        let value = serde_json::from_str(&text).map_err(|_| "Invalid tool response.")?;
                        super::information_flow::record_response(&context, &value)?;
                    }
                    Ok(text)
                });
                let (summary, ok) = match &result {
                    Ok(_) => (String::new(), true),
                    Err(e) => (e.clone(), false),
                };
                match &result {
                    Ok(text) => log!(
                        "AI Rooms: room {}'s {} tool answered OK ({} chars).",
                        room_id,
                        read_tool_name(&kind),
                        text.chars().count()
                    ),
                    Err(e) => log!(
                        "AI Rooms: room {}'s {} tool answered Err: {}",
                        room_id,
                        read_tool_name(&kind),
                        e
                    ),
                }
                // Every outcome rewrites the call's live `ai_tool_call` row
                // to `Done`; a granted read's outcome also rides on the
                // turn's receipt chip. Ungated memory reads (recalling its
                // own past turns) are room plumbing, not something the user
                // watches for, so they leave no chip — only their row.
                let detail = finish_ai_tool_call(&room_id, read_tool_name(&kind), ok, &summary);
                if !matches!(kind, ReadToolKind::Memory { .. }) {
                    push_ai_tool_receipt(&room_id, read_tool_name(&kind), detail, ok, &summary);
                }
                let _ = answer.send(result);
            }
        }
        AiRoomAction::StateEventPosted { room_id, event_type, write_id, success } => {
            // Release the coalescing guard for this room's turn card so the
            // next pending snapshot can be written (see
            // `flush_pending_ai_turns`), and adapt the write spacing: a rate
            // limit widens it, a success narrows it back. Only the captured
            // write id may release this gate; other event types release their
            // own final-write tokens below.
            if event_type.as_str() == AI_TURN_EVENT_TYPE {
                with_a2app(|state| {
                    if let Some(info) = state.ai_rooms.get_mut(&room_id) {
                        if info.ai_turn_write_id != Some(write_id) { return; }
                        info.ai_turn_in_flight = false;
                        info.ai_turn_write_id = None;
                        if let Some(key) = info.anchor_in_flight.take()
                            && !success
                            && info.first_posted_turn.as_deref() == Some(key.as_str())
                        {
                            info.first_posted_turn = None;
                        }
                        if success {
                            info.ai_turn_backoff =
                                (info.ai_turn_backoff / 2).max(AI_TURN_POST_MIN_INTERVAL);
                        } else {
                            info.ai_turn_backoff =
                                (info.ai_turn_backoff * 2).min(AI_TURN_POST_MAX_INTERVAL);
                        }
                    }
                });
            }
            // A closed turn's final write releases its token when it lands.
            final_write_landed(&room_id, write_id);
        }
    }
}

/// Tears down a session that just died. A `Gone` session's final Stopped row
/// and `Done` snapshot still need its flow context, so if a closed turn has
/// final writes outstanding the teardown is deferred ([`settle_closed_turn`]
/// runs it once they land). Every other path stops at once.
#[cfg(unix)]
fn retire_ai_session(room_id: &OwnedRoomId) {
    let deferred = with_a2app(|state| {
        let info = state.ai_rooms.get_mut(room_id)?;
        if info.final_writes_pending_revoke
            && (!info.pending_final_turns.is_empty() || !info.pending_final_writes.is_empty())
        {
            info.retire_after_final_writes = true;
            Some(true)
        } else {
            Some(false)
        }
    })
    .flatten()
    .unwrap_or(false);
    if !deferred {
        stop_ai_session(room_id);
    }
}

/// Stops an AI room's session and resets what a restart needs (one-time
/// grants, in-flight reads). Grants and restrictions — the user's durable
/// answers — are kept. Mirrors what the death path does, minus the popups;
/// used by the panel's power switch and explicit Stop. The next prompt starts
/// a fresh activation with the same durable provenance.
#[cfg(unix)]
fn stop_ai_session(room_id: &OwnedRoomId) {
    #[cfg(unix)]
    {
        // Task grants are turn-scoped; the session stopping ends them, and a
        // pending task prompt is answered declined so its caller never hangs.
        revoke_task_grants(room_id);
        for prompt in take_room_tasks(room_id) {
            if let TaskResume::AgentTool(answer) = prompt.resume {
                let message = task_grants::outcome(&prompt.plan, &std::collections::BTreeSet::new()).to_string();
                let _ = answer.send(Ok(message));
            }
        }
    }
    cancel_ai_fetches(room_id);
    // Drop the exact session before waking tool callers or processing more
    // UI work. Its Drop retires only its captured account and activation;
    // deriving a fresh context here could revoke an unrelated replacement.
    let session = with_a2app(|state| state.ai_sessions.remove(room_id)).flatten();
    drop(session);
    with_a2app(|state| {
        state.ai_reads.retain(|_, (r, _, _)| r != room_id);
        state.ai_posts.retain(|_, (r, _)| r != room_id);
        state.ai_replies.retain(|_, (r, _, _, _)| r != room_id);
        // Answer any app-tool invocation parked on this session; the tools
        // themselves stay in `app_tools` so a restarted session can re-install
        // them without re-prompting (see `attach_ai_session`).
        let ids: Vec<u64> = state
            .app_tool_calls
            .iter()
            .filter(|(_, p)| &p.room_id == room_id)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(p) = state.app_tool_calls.remove(&id) {
                let _ = p.answer.send(Err(String::from("this room's AI session stopped")));
            }
        }
        let subject = agent_subject(room_id.as_str());
        state.permissions.clear_once_for(&subject);
        state.once_rooms.retain(|(s, _, _)| s != &subject);
        state.perms_dirty = true;
        // The turn died with the session: nothing of it may leak into the
        // next session's first reply.
        if let Some(info) = state.ai_rooms.get_mut(room_id) {
            info.discard_session_work();
        }
    });
    // The timeline settles any card that is no longer the active turn. Do
    // not queue a final snapshot that could outlive this retired activation.
}

/// The panel's power switch: on (re)attaches the agent, off stops it and the
/// forwarding that would revive it.
#[cfg(unix)]
fn set_ai_room_power(cx: &mut Cx, ui: &WidgetRef, room_id: &OwnedRoomId, on: bool) {
    with_a2app(|state| {
        if let Some(info) = state.ai_rooms.get_mut(room_id) {
            info.session_on = on;
        }
    });
    if on {
        log!("AI Rooms: powering room {room_id}'s AI back on; attaching a session.");
        attach_ai_session(cx, ui, room_id, None);
    } else {
        log!("AI Rooms: powering off room {room_id}'s AI.");
        abort_ai_room_work(cx, ui, room_id);
    }
    ui.redraw(cx);
}

/// The `/ai` command: with no argument it opens the "AI in this room" panel;
/// `enable` marks the current room as an AI room. Reads of other rooms are
/// asked for at the ordinary permission prompt — there is no allowlist to
/// maintain. a2app + unix only (other builds no-op the action).
#[cfg(unix)]
pub fn ai_panel_command(cx: &mut Cx, arg: &str, room_id: &OwnedRoomId) {
    let known = with_a2app(|state| state.ai_rooms.contains_key(room_id)).unwrap_or(false);
    let arg = arg.trim();

    // `/ai enable` writes the marker to an existing room, turning it into an
    // AI room; the room's agent attaches like any marked room.
    if arg == "enable" {
        if known {
            enqueue_popup_notification(
                "This room is already an AI room.",
                PopupKind::Warning, Some(4.0),
            );
            return;
        }
        submit_async_request(MatrixRequest::AiRoom(AiRoomRequest::Mark {
            room_id: room_id.clone(),
        }));
        enqueue_popup_notification(
            "Marking this room as an AI room...",
            PopupKind::Info, Some(4.0),
        );
        return;
    }

    if !known {
        enqueue_popup_notification(
            "This isn't an AI room — there is no AI here to manage (try /ai enable).",
            PopupKind::Warning,
            Some(5.0),
        );
        return;
    }
    if !arg.is_empty() {
        enqueue_popup_notification(
            "Usage: /ai — or /ai enable to turn this room into an AI room.",
            PopupKind::Warning,
            Some(5.0),
        );
        return;
    }
    cx.action(A2AppOp::AiRoomPanel(room_id.clone()));
}

/// Opens the room's AI panel (from an op), or refreshes it after an action.
#[cfg(unix)]
fn open_ai_room_panel(cx: &mut Cx, ui: &WidgetRef, room_id: &OwnedRoomId) {
    if !with_a2app(|state| state.ai_rooms.contains_key(room_id)).unwrap_or(false) {
        enqueue_popup_notification(
            "This isn't an AI room — there is no AI here to manage.",
            PopupKind::Warning,
            Some(5.0),
        );
        return;
    }
    refresh_ai_room_panel(cx, ui, room_id);
}

/// The panel's rows, in the order its widget lays them out.
#[cfg(unix)]
const AI_PANEL_PERMS: [Permission; 10] = [
    Permission::MatrixRoomRead,
    Permission::MatrixRoomInfo,
    Permission::AppGeneration,
    Permission::AppLaunch,
    Permission::MatrixRoomsRead,
    Permission::MatrixRoomsList,
    Permission::MatrixSpaces,
    Permission::MatrixRoomsSend,
    Permission::McpTools,
    Permission::Network,
];

/// Whether the panel's row for `perm` is only a kill switch: the group's
/// `Granted` opens nothing by itself, because the gate asks per room
/// ([`run_ai_room_post`]), per tool ([`run_mini_app_tool_call`]) or per site
/// ([`run_network_access`]). Such a row offers no "Allow" button.
#[cfg(unix)]
fn ai_panel_is_kill_switch(perm: Permission) -> bool {
    matches!(
        perm,
        Permission::MatrixRoomsSend | Permission::McpTools | Permission::Network
    )
}

/// A group's name as the AI panel says it: the catalog titles are written for
/// a mini-app declaring the permission, and a couple read wrong for an agent
/// being granted it.
#[cfg(unix)]
fn ai_panel_title(perm: Permission) -> &'static str {
    match perm {
        Permission::McpTools => "Use mini-app tools",
        Permission::Network => "Reach the internet",
        other => other.title(),
    }
}

/// Repopulates and shows the AI room panel for `room_id` with current state.
#[cfg(unix)]
fn refresh_ai_room_panel(cx: &mut Cx, ui: &WidgetRef, room_id: &OwnedRoomId) {
    let rooms = cx.has_global::<RoomsListRef>().then(|| cx.get_global::<RoomsListRef>().clone());
    let room_name = rooms
        .as_ref()
        .and_then(|r| room_display_name(r, room_id.as_str()))
        .unwrap_or_else(|| room_id.to_string());
    let subject = agent_subject(room_id.as_str());
    let Some(info) = with_a2app(|state| {
        let info = state.ai_rooms.get(room_id)?;
        let perm_word = |perm: Permission| match state
            .permissions
            .effective_for_in_context(&subject, ai_room_declares_perm, perm, PermissionContext {
                origin_room: Some(room_id.as_str()), target_room: Some(room_id.as_str()),
            })
        {
            // These groups only ever act as a kill switch: what the AI may
            // actually do is decided per room, per tool or per site, so a
            // group grant must not read as blanket permission.
            Effective::Granted if ai_panel_is_kill_switch(perm) => "ask each time",
            Effective::Granted => "allowed",
            Effective::Denied => "don't allow",
            Effective::NeedsPrompt => "ask each time",
            Effective::Undeclared => "not offered",
        };
        let line = |perm: Permission| {
            let count = state.permissions.use_count(&subject, perm);
            let mut s = format!("{} — {}", ai_panel_title(perm), perm_word(perm));
            if count > 0 {
                s.push_str(&format!(" · {count} use{}", if count == 1 { "" } else { "s" }));
                if let Some(at) = state.permissions.last_access(&subject, perm) {
                    let now = versions::now_unix();
                    let ago = now.saturating_sub(at);
                    s.push_str(" · last ");
                    s.push_str(&if ago < 90 {
                        "just now".to_string()
                    } else if ago < 3600 {
                        format!("{}m ago", ago / 60)
                    } else if ago < 86_400 {
                        format!("{}h ago", ago / 3600)
                    } else {
                        format!("{}d ago", ago / 86_400)
                    });
                }
            }
            s
        };
        // Every group this room's AI can be asked about, so a prompt's Deny
        // is always undoable here. The panel maps each line to a fixed row,
        // so a group this AI does not offer yields an empty line (which hides
        // that row) rather than shifting every row after it.
        let rows: Vec<String> = AI_PANEL_PERMS
            .iter()
            .map(|p| if ai_room_declares_perm(*p) { line(*p) } else { String::new() })
            .collect();
        let managed: Vec<Permission> = AI_PANEL_PERMS
            .into_iter()
            .filter(|p| ai_room_declares_perm(*p))
            .collect();
        // The one-at-a-time grants, which no group row covers: without this
        // there is no way back from an "allow this room / this site" answer.
        let extras = {
            let name_of = |id: &str| {
                rooms
                    .as_ref()
                    .and_then(|r| room_display_name(r, id))
                    .unwrap_or_else(|| id.to_string())
            };
            let names = |ids: Vec<String>| {
                ids.iter().map(|id| name_of(id)).collect::<Vec<_>>().join(", ")
            };
            let mut parts: Vec<String> = Vec::new();
            let send = state.permissions.room_send_grants(&subject);
            if !send.is_empty() {
                parts.push(format!("post into {}", names(send)));
            }
            let read = state.permissions.room_read_grants(&subject);
            if !read.is_empty() {
                parts.push(format!("read {}", names(read)));
            }
            let hosts = state.permissions.host_grants(&subject);
            if !hosts.is_empty() {
                parts.push(format!("reach {}", hosts.join(", ")));
            }
            let tools = state.permissions.tool_grants(&subject).len();
            if tools > 0 {
                parts.push(format!(
                    "call {tools} mini-app tool{}",
                    if tools == 1 { "" } else { "s" }
                ));
            }
            let scoped = state.permissions.scoped_grants(&subject).len() + state.permissions.network_grants(&subject).len();
            if scoped > 0 {
                parts.push(format!("{scoped} scoped permission rules (manage them in Mini Apps)"));
            }
            let mut line = if parts.is_empty() {
                String::new()
            } else {
                format!("Also allowed, one at a time: {}.", parts.join(" · "))
            };
            // An "Allow once" / "Don't allow" answer is listed nowhere else,
            // and forgetting it is the only way back, so it has to bring the
            // row (and its button) up on its own.
            if state.permissions.has_session_answers(&subject)
                || state.once_rooms.iter().any(|(s, _, _)| s == &subject)
            {
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str("Some one-time answers still apply for this session.");
            }
            (!line.is_empty()).then_some(line)
        };
        let usage = {
            let parts: Vec<String> = managed
                .iter()
                .filter_map(|p| {
                    let n = state.permissions.use_count(&subject, *p);
                    (n > 0).then(|| format!("{} {n}×", p.title().to_lowercase()))
                })
                .collect();
            if parts.is_empty() {
                String::from("No tool use recorded yet.")
            } else {
                format!("Recent use: {}", parts.join(", "))
            }
        };
        let restriction = state
            .permissions
            .restriction(&subject)
            .map(|r| format!("The host stopped this room's AI: {}.", r.reason));
        Some(AiRoomPanelInfo {
            room_id: room_id.to_string(),
            room_name,
            powered_on: info.session_on,
            rows,
            extras,
            usage,
            restriction,
        })
    })
    .flatten()
    else {
        return;
    };
    ui.ai_room_panel(cx, ids!(ai_room_panel_modal.content)).show(cx, &info);
    ui.modal(cx, ids!(ai_room_panel_modal)).open(cx);
}

/// Applies one AI-room panel action (a group answer, a power flip, or an
/// unrestrict) and re-shows the panel with the new state.
#[cfg(unix)]
fn apply_ai_room_panel_action(cx: &mut Cx, ui: &WidgetRef, action: AiRoomPanelAction) {
    if matches!(action.command, AiRoomPanelCommand::Close) {
        ui.modal(cx, ids!(ai_room_panel_modal)).close(cx);
        return;
    }
    let Ok(room_id) = OwnedRoomId::try_from(action.room_id.as_str()) else { return };
    let subject = agent_subject(&action.room_id);
    match (action.perm, action.command) {
        (Some(perm), AiRoomPanelCommand::Allow) => {
            with_a2app(|state| {
                state.permissions.set(&subject, perm, GrantState::Granted);
                state.perms_dirty = true;
            });
        }
        (Some(perm), AiRoomPanelCommand::Ask) => {
            permission_lifecycle::ask_again(cx, ui, &subject, perm);
            return;
        }
        (Some(perm), AiRoomPanelCommand::Deny) => {
            with_a2app(|state| {
                state.permissions.set(&subject, perm, GrantState::Denied);
                state.perms_dirty = true;
            });
        }
        (None, AiRoomPanelCommand::ForgetExtras) => {
            with_a2app(|state| {
                let store = &mut state.permissions;
                for room in store.room_send_grants(&subject) {
                    store.disallow_room_send(&subject, &room);
                }
                for room in store.room_read_grants(&subject) {
                    store.disallow_room_read(&subject, &room);
                }
                for host in store.host_grants(&subject) {
                    store.disallow_host(&subject, &host);
                }
                store.clear_tool_grants_for(&subject);
                let grants: Vec<_> = store.scoped_grants(&subject).iter().map(|g| g.id).collect();
                for id in grants { store.remove_scoped_grant(id); }
                let grants: Vec<_> = store.network_grants(&subject).iter().map(|g| g.id).collect();
                for id in grants { store.remove_network_grant(id); }
                // The one-time answers go too: a host or group allowed once is
                // checked before any durable grant, so leaving it would make
                // the confirmation a lie.
                store.clear_once_for(&subject);
                state.once_rooms.retain(|(s, _, _)| s != &subject);
                state.perms_dirty = true;
            });
            enqueue_popup_notification(
                "Forgotten. This room's AI asks again before it uses any of them.",
                PopupKind::Info,
                Some(4.0),
            );
        }
        (None, AiRoomPanelCommand::Unrestrict) => {
            with_a2app(|state| {
                state.permissions.unrestrict(&subject);
                state.perms_dirty = true;
            });
            enqueue_popup_notification(
                "This room's AI can run again. It will ask you before anything new.",
                PopupKind::Info,
                Some(4.0),
            );
        }
        (None, AiRoomPanelCommand::PowerOn) => set_ai_room_power(cx, ui, &room_id, true),
        (None, AiRoomPanelCommand::PowerOff) => set_ai_room_power(cx, ui, &room_id, false),
        _ => return,
    }
    publish_grants(cx);
    refresh_ai_room_panel(cx, ui, &room_id);
    ui.redraw(cx);
}

/// The four directory tools whose reads the agent gets by default, so it can
/// name real rooms in an upfront request without asking first. Directory
/// results carry no message content but stay untrusted input.
#[cfg(unix)]
fn is_directory_kind(kind: &ReadToolKind) -> bool {
    kind.is_directory()
}

/// Grants an AI room the room/space directory (`list_rooms`, `list_spaces`,
/// `space_info`, `list_space_rooms`) once, on its first session start, unless
/// the user has an explicit answer for a group. A stored Deny is the kill
/// switch and is never overridden; the persisted per-room marker means a user
/// who sets a group back to `Ask` or `Deny` keeps that choice on every later
/// session. The room's own transcript and metadata, and the installed-app
/// list, are not defaulted: they go through `request_task_permissions`.
#[cfg(unix)]
fn apply_ai_room_capability_defaults(state: &mut A2AppState, room_id: &OwnedRoomId) {
    if !state.persisted.permission_defaults_applied.insert(room_id.to_string()) {
        return;
    }
    state.registry_dirty = true;
    let changed = apply_directory_capability_defaults(&mut state.permissions, room_id);
    if changed {
        state.mark_perms_dirty();
    }
}

/// Applies the one-time directory defaults to a permission store. Returns
/// whether anything changed. Split out from [`apply_ai_room_capability_defaults`]
/// so the once-per-room behavior is unit-testable.
///
/// The directory capabilities share permission groups with other capabilities
/// (room search, previews, invites, the room-list hooks; the space-changed
/// hook), so the group itself is left `Ask`: each directory capability is
/// granted by id with a durable all-rooms scoped grant. Setting the group to
/// `Granted` would silently hand the agent everything else in it.
#[cfg(unix)]
fn apply_directory_capability_defaults(permissions: &mut PermissionStore, room_id: &OwnedRoomId) -> bool {
    let subject = agent_subject(room_id.as_str());
    let mut changed = false;
    for cap_id in a2app_core::task_grants::DIRECTORY_CAP_IDS {
        let Some(cap) = a2app_core::capabilities::by_id(cap_id) else { continue };
        let Some(group) = cap.group else { continue };
        // Saved answers and narrowed or withdrawn allowances predate this
        // default on upgrade. Preserve them instead of widening their scope.
        if permissions.has_capability_choice(&subject, cap) {
            continue;
        }
        let already = permissions.scoped_grants(&subject).iter().any(|grant| {
            grant.capability.as_deref() == Some(*cap_id) && grant.scope == RoomScope::AllRooms
        });
        if already {
            continue;
        }
        match permissions.grant_scoped(
            &subject,
            group,
            Some(cap_id),
            RoomScope::AllRooms,
            GrantDuration::Always,
            None,
        ) {
            Ok(_) => changed = true,
            Err(error) => log!("AI Rooms: couldn't default the {cap_id} capability: {error}"),
        }
    }
    changed
}

/// Starts an AI room's session if it isn't already running. Started idle
/// (no prompt sent yet) — the first forwarded message primes it with the
/// replayed transcript and sends it as one prompt.
#[cfg(unix)]
fn attach_ai_session(cx: &mut Cx, ui: &WidgetRef, room_id: &OwnedRoomId, _name: Option<String>) {
    if !room_policy_allows(room_id.as_str(), RoomAccess::Read) { return; }
    // Turned off from the panel: don't (re)start the agent.
    let powered_off = with_a2app(|state| {
        state.ai_rooms.get(room_id).map(|info| !info.session_on).unwrap_or(false)
    }).unwrap_or(false);
    if powered_off {
        log!("AI Rooms: room {room_id}'s AI is turned off; not attaching a session.");
        return;
    }
    let already_running = with_a2app(|state| state.ai_sessions.contains_key(room_id)).unwrap_or(true);
    if already_running {
        log!("AI Rooms: room {room_id}'s agent session is already running; keeping it.");
        return;
    }
    let prefs = with_a2app(|state| state.agent_prefs.clone())
        .unwrap_or_else(a2app_agent::prefs::load_agent_prefs);
    let started = with_a2app(|state| {
        let started = start_ai_session(state, room_id, prefs).map(|session| {
            state.ai_sessions.insert(room_id.clone(), session);
        });
        if let Some(info) = state.ai_rooms.get_mut(room_id) {
            info.start_failed_at = started.is_err().then(Instant::now);
        }
        started
    });
    if let Some(Err(e)) = started {
        log!("AI Rooms: FAILED to start the agent session for AI room {room_id}: {e}");
        enqueue_popup_notification(
            format!("Couldn't start this AI room's agent: {e}"),
            PopupKind::Error, Some(6.0),
        );
    } else {
        log!("AI Rooms: started the agent session for AI room {room_id}.");
    }
    ui.redraw(cx);
}

/// Restore reviewed tools and their current provenance on every fresh session,
/// including the next member prompt after an explicit Stop.
#[cfg(unix)]
fn start_ai_session(state: &mut A2AppState, room_id: &OwnedRoomId, prefs: AgentPrefs) -> Result<AiSession, String> {
    let context = super::information_flow::begin_agent_session(room_id.as_str())?;
    let epoch = a2app_core::information_flow::context_epoch(&context)?;
    // A fresh room must not have to prompt for the room/space directory. The
    // other directory-adjacent capabilities (its own transcript, the installed
    // mini-app list) still go through `request_task_permissions`. Applied here
    // (not only on the marker-check attach path) so a lazily started session —
    // the first message in a newly created room — gets the directory defaults
    // too.
    apply_ai_room_capability_defaults(state, room_id);
    let result = (|| {
        // A freshly created AI room has no sharing rules yet, so its first
        // model call and its own activity/reply cards would be refused with
        // "Information flow blocked". Grant the baseline it cannot work
        // without, so an AI room works out of the box (the user can still
        // revoke these in the Data Sharing editor).
        let model = a2app_agent::model_transport::current_recipient(&prefs)
            .ok()
            .map(|recipient| recipient.id);
        let homeserver = crate::sliding_sync::get_client()
            .map(|client| client.homeserver().to_string());
        if super::information_flow::ensure_agent_default_sharing(
            &context, room_id.as_str(), model.as_deref(), homeserver.as_deref(),
            &mut state.persisted.default_sharing_applied,
        ) {
            state.registry_dirty = true;
        }
        for reg in state.app_tools.values().filter(|reg| &reg.room_id == room_id) {
            transfer_app_tool_provenance(state, &reg.app_id, reg.heap_key, room_id)?;
        }
        let session = AiSession::start(room_id.clone(), prefs, context.clone())?;
        for reg in state.app_tools.values().filter(|reg| &reg.room_id == room_id) {
            transfer_app_tool_provenance(state, &reg.app_id, reg.heap_key, room_id)?;
            session.register_miniapp_tool(reg.full_name.clone(), reg.description.clone(), reg.schema.clone())?;
        }
        Ok(session)
    })();
    if result.is_err() {
        let _ = a2app_core::information_flow::remove_context_for_activation(&context, epoch);
    }
    result
}

/// What `room_screen.rs` needs to scan an AI room's currently-loaded
/// timeline items for new member messages to forward.
#[cfg(unix)]
pub struct AiRoomScanState {
    /// Only items after this event (by timeline position) are new.
    /// `None` means the room has never forwarded anything yet.
    pub cursor: Option<OwnedEventId>,
}

/// Returns this room's forwarding state, or `None` if it isn't (or isn't yet
/// known to be) an AI room, or its AI is turned off — the caller's cue to
/// skip scanning entirely.
#[cfg(unix)]
pub fn ai_room_scan_state(room_id: &OwnedRoomId) -> Option<AiRoomScanState> {
    with_a2app(|state| {
        state.ai_rooms.get(room_id).filter(|info| info.session_on).map(|info| AiRoomScanState {
            cursor: info.cursor.clone(),
        })
    }).flatten()
}

/// The live activity of a room's agent session, for its in-room status row:
/// whether a turn is in flight, and how many member prompts are queued
/// behind it (waiting for the agent to be free, or for it to finish
/// starting). `None` when the room has no live session — the row is hidden.
#[cfg(unix)]
pub struct AiRoomStatus {
    pub busy: bool,
    pub queued: usize,
}

/// Reads a room's session activity; see [`AiRoomStatus`].
#[cfg(unix)]
pub fn ai_room_status(room_id: &OwnedRoomId) -> Option<AiRoomStatus> {
    with_a2app(|state| {
        let session = state.ai_sessions.get(room_id)?;
        Some(AiRoomStatus {
            busy: session.is_busy(),
            queued: session.queued_len() + state.ai_rooms.get(room_id).map(|info| info.pending_member_prompts.len()).unwrap_or(0),
        })
    })
    .flatten()
}

/// Whether the room's AI session is mid-work right now: a turn in flight, or
/// member prompts queued behind it. The room's Escape-to-abort gate — when
/// false there is nothing for the user to stop.
#[cfg(unix)]
pub fn ai_room_is_busy(room_id: &OwnedRoomId) -> bool {
    with_a2app(|state| {
        state.ai_sessions.get(room_id).map(|s| s.is_busy() || s.queued_len() > 0
            || state.ai_rooms.get(room_id).is_some_and(|info| !info.pending_member_prompts.is_empty()))
    })
    .flatten()
    .unwrap_or(false)
}

/// The state key of the room's currently-open turn card, if any. The timeline
/// renderer uses this to decide whether a turn is still running: a card whose
/// turn is no longer the active one is settled, even if its final `Done`
/// snapshot never landed (e.g. the app was restarted mid-turn, or the room was
/// re-attached, so the last snapshot still says `Running`).
#[cfg(unix)]
pub fn ai_room_active_turn(room_id: &OwnedRoomId) -> Option<String> {
    with_a2app(|state| {
        state.ai_rooms.get(room_id).and_then(|info| {
            info.active_turn.as_ref().map(|turn| turn.key.clone())
        })
    })
    .flatten()
}

/// No AI sessions off unix, so no turn is ever active.
#[cfg(not(unix))]
pub fn ai_room_active_turn(_room_id: &OwnedRoomId) -> Option<String> {
    None
}

/// Stops this room's agent activation and its pending work.
///
/// Retiring the session closes its tool queue and invalidates requests already
/// sent to workers. A later prompt starts a fresh session; durable source
/// labels remain. Effects already committed while allowed cannot be recalled.
/// Only the room's own generation is touched — a build the Mini Apps screen
/// (or another room) started keeps running.
#[cfg(unix)]
fn abort_ai_room_work(cx: &mut Cx, ui: &WidgetRef, room_id: &OwnedRoomId) {
    // Explicit cancellation forgets held asks too. Keep the cursor at the
    // newest canceled event so the next timeline pass does not restart them.
    // Session death and failed startup keep their old cursor for refetch.
    if let Some(cursor) = discard_member_prompts_for_cancel(room_id)
        && let Ok(account) = super::information_flow::account()
    {
        submit_async_request(MatrixRequest::AiRoom(AiRoomRequest::SaveCancellationCursor {
            account, room_id: room_id.clone(), cursor,
            write_id: NEXT_AI_WRITE_ID.fetch_add(1, Ordering::Relaxed),
        }));
    }
    // Withdraw a task prompt first, so the modal closes and no serve thread
    // hangs on a task whose turn is being cancelled.
    #[cfg(unix)]
    refuse_room_tasks(cx, ui, room_id);
    // Retire before answering any parked tool: a still-running blocking
    // caller must not enqueue a new UI job after we drain the old queue.
    stop_ai_session(room_id);
    with_a2app(|state| {
        if state.pending_generated.as_ref().is_some_and(|pending| pending.context.room() == Some(room_id.as_str())) {
            state.pending_generated = None;
            state.failed_request = None;
            state.console.status = "Cancelled.".into();
        }
    });
    // Cancel the room's own generation, if one is running. Exactly the Mini
    // Apps screen's Stop, but scoped to this room's build.
    let cancels_generation = with_a2app(|state| {
        state.ai_generation_room.as_ref().is_some_and(|r| r == room_id)
            && state.generation.is_some()
    })
    .unwrap_or(false);
    if cancels_generation {
        with_a2app(|state| {
            // Dropping the Generation kills its agent child process.
            state.generation = None;
            state.generation_context = None; state.generation_epoch = None;
            state.console.status = String::from("Cancelled.");
        });
        resolve_session_generation(
            cx,
            ui,
            None,
            Err(String::from("The generation was cancelled.")),
        );
    }
    // A prompt parked by the cancelled turn would run its tool call (post,
    // build, launch) whenever it was answered; withdraw it.
    refuse_room_prompts(cx, ui, room_id);
    ui.redraw(cx);
}

/// Forwards newly-seen member messages (in timeline order) to an AI room's
/// session as prompts, attaching the session first if needed. Nothing is sent
/// here beyond the member messages themselves: a session starts fresh and
/// silent, and its first (and only) inputs are real user messages.
///
/// Before starting a turn, if the agent's still-live label carries a source
/// the model provider, this room or its homeserver may no longer receive (a
/// room read in an earlier turn whose sharing rule was revoked), the host raises one carried-over
/// task prompt and holds these messages until the user answers. On approval
/// the host applies the same turn-scoped rules and delivers the messages; on
/// a decline the messages are still delivered, so the model call is refused
/// the standard way.
///
/// Each successfully-forwarded event becomes the room's new cursor,
/// persisted as room account data for restart continuity.
#[cfg(unix)]
pub fn forward_ai_room_texts(
    room_id: &OwnedRoomId,
    new_texts: Vec<(OwnedEventId, String)>,
) {
    forward_ai_room_texts_inner(room_id, new_texts, true);
}

#[cfg(unix)]
fn queue_member_prompts(info: &mut AiRoomInfo, texts: Vec<(OwnedEventId, String)>) {
    for (event_id, text) in texts {
        if info.cursor.as_ref() == Some(&event_id)
            || info.pending_member_prompts.iter().any(|(held, _)| held == &event_id)
        { continue; }
        if info.pending_member_prompts.len() >= super::ai::session::MAX_QUEUED_PROMPTS {
            if let Some((forgotten, _)) = info.pending_member_prompts.pop_front() {
                info.cursor = Some(forgotten);
            }
        }
        info.pending_member_prompts.push_back((event_id, text));
    }
}

#[cfg(unix)]
fn discard_member_prompts_for_cancel(room_id: &OwnedRoomId) -> Option<OwnedEventId> {
    with_a2app(|state| {
        let info = state.ai_rooms.get_mut(room_id)?;
        let cursor = info.pending_member_prompts.back()?.0.clone();
        info.cursor = Some(cursor.clone());
        info.pending_member_prompts.clear();
        Some(cursor)
    }).flatten()
}

#[cfg(unix)]
fn member_prompt_may_start(info: &AiRoomInfo, session_busy: bool, session_queued: usize) -> bool {
    !session_busy && session_queued == 0 && !info.final_writes_pending_revoke
}

#[cfg(unix)]
fn send_next_member_prompt(
    info: &mut AiRoomInfo,
    session_busy: bool,
    session_queued: usize,
    send: impl FnOnce(String) -> PromptOutcome,
) -> Option<OwnedEventId> {
    if !member_prompt_may_start(info, session_busy, session_queued) { return None; }
    let (event_id, text) = info.pending_member_prompts.front()?.clone();
    if send(text) != PromptOutcome::Sent { return None; }
    info.pending_member_prompts.pop_front();
    info.cursor = Some(event_id.clone());
    Some(event_id)
}

#[cfg(unix)]
fn forward_ai_room_texts_inner(
    room_id: &OwnedRoomId,
    new_texts: Vec<(OwnedEventId, String)>,
    check_carried: bool,
) {
    if new_texts.is_empty() {
        return;
    }
    if !room_policy_allows(room_id.as_str(), RoomAccess::Read) {
        return;
    }
    log!("AI Rooms: forwarding {} new message(s) to room {room_id}'s session...", new_texts.len());

    // A start that just failed is not retried (with its popup) on every
    // timeline update; the messages stay unforwarded until the cooldown ends.
    let cooling = with_a2app(|state| {
        !state.ai_sessions.contains_key(room_id)
            && state
                .ai_rooms
                .get(room_id)
                .and_then(|info| info.start_failed_at)
                .is_some_and(|at| at.elapsed() < AI_START_RETRY_COOLDOWN)
    })
    .unwrap_or(false);
    if cooling {
        log!("AI Rooms: room {room_id}'s agent failed to start recently; not retrying yet.");
        return;
    }
    // Member prompts stay here until startup and final-write rollback have
    // completed, so neither a model call nor its host tools can inherit the old grants.
    if let Err(error) = ensure_ai_session(room_id) {
        log!("AI Rooms: FAILED to start/use room {room_id}'s agent session: {error}");
        enqueue_popup_notification(
            format!("Couldn't start this AI room's agent: {error}"),
            PopupKind::Error, Some(6.0),
        );
        return;
    }
    let texts = with_a2app(|state| {
        let blocked = session_has_inflight_tool(state, room_id);
        let session = state.ai_sessions.get(room_id)?;
        let (busy, queued, dead, ready) = (session.is_busy(), session.queued_len(), session.dead(), session.is_ready());
        let info = state.ai_rooms.get_mut(room_id)?;
        queue_member_prompts(info, new_texts);
        let may_start = member_prompt_may_start(info, busy, queued);
        let texts = info.pending_member_prompts.iter().cloned().collect::<Vec<_>>();
        update_pending_carried_texts(state, room_id, &texts);
        if dead || !ready || blocked || !may_start { return None; }
        Some(texts)
    }).flatten();
    let Some(texts) = texts.filter(|texts| !texts.is_empty()) else { return };
    if check_carried && raise_carried_permission_prompt(room_id, &texts) {
        log!("AI Rooms: holding {} message(s) for room {room_id} behind a carried-over permission prompt.", texts.len());
        return;
    }
    let sent = with_a2app(|state| {
        let session = state.ai_sessions.get_mut(room_id)?;
        let info = state.ai_rooms.get_mut(room_id)?;
        send_next_member_prompt(info, session.is_busy(), session.queued_len(), |text| session.prompt(text))
    }).flatten();
    if let Some(cursor) = sent {
        log!("AI Rooms: forwarded {cursor} to room {room_id}'s session.");
        let Ok(flow_context) = super::information_flow::agent_context(room_id.as_str()) else { return };
        submit_async_request(MatrixRequest::AiRoom(AiRoomRequest::SaveCursor {
            write_id: NEXT_AI_WRITE_ID.fetch_add(1, Ordering::Relaxed),
            flow_epoch: a2app_core::information_flow::context_epoch(&flow_context).unwrap_or(0),
            flow_context,
            room_id: room_id.clone(),
            cursor,
        }));
    }
}

/// Recheck carried sharing after the preceding turn's final grants have been
/// removed, then deliver at most one held member message per idle room.
#[cfg(unix)]
fn drain_pending_member_prompts() {
    let ready: Vec<(OwnedRoomId, Vec<(OwnedEventId, String)>)> = with_a2app(|state| {
        state.ai_rooms.iter().filter_map(|(room_id, info)| {
            let session = state.ai_sessions.get(room_id)?;
            if info.pending_member_prompts.is_empty() || session.dead() || !session.is_ready()
                || !member_prompt_may_start(info, session.is_busy(), session.queued_len())
                || session_has_inflight_tool(state, room_id)
            { return None; }
            Some((room_id.clone(), info.pending_member_prompts.iter().cloned().collect()))
        }).collect()
    }).unwrap_or_default();
    for (room_id, texts) in ready {
        forward_ai_room_texts_inner(&room_id, texts, true);
    }
}

/// Attaches a room's AI session if it has none, so a carried-over check can
/// see the (possibly reset) label before a turn starts.
#[cfg(unix)]
fn ensure_ai_session(room_id: &OwnedRoomId) -> Result<(), String> {
    if with_a2app(|state| state.ai_sessions.contains_key(room_id)).unwrap_or(false) {
        return Ok(());
    }
    let prefs = with_a2app(|state| state.agent_prefs.clone())
        .unwrap_or_else(a2app_agent::prefs::load_agent_prefs);
    with_a2app(|state| {
        let started = start_ai_session(state, room_id, prefs);
        if let Some(info) = state.ai_rooms.get_mut(room_id) {
            info.start_failed_at = started.is_err().then(Instant::now);
        }
        state.ai_sessions.insert(room_id.clone(), started?);
        Ok(())
    })
    .unwrap_or_else(|| Err("AI state is unavailable.".into()))
}

/// The rows a host-raised carried-over prompt shows: every source beyond the
/// baseline that has no rule to the model provider, to the homeserver origin
/// or back into the room. The explanation names what was read.
#[cfg(unix)]
fn build_carried_task_plan(
    room_id: &OwnedRoomId,
    context: &a2app_core::information_flow::ContextId,
    sources: &BTreeSet<Source>,
    provider: &Recipient,
    homeserver: Option<&Recipient>,
    task_id: u64,
) -> TaskPlan {
    use a2app_core::capabilities::Risk;
    use a2app_core::task_grants::{ItemOrigin, PlanAction, PlanItem};
    let account = context.account().to_string();
    let own_room = Recipient::MatrixRoom { account, room: room_id.to_string() };
    let mut recipients = vec![provider.clone()];
    if let Some(homeserver) = homeserver {
        recipients.push(homeserver.clone());
    }
    recipients.push(own_room);
    let mut items = Vec::new();
    let mut labels = Vec::new();
    for source in sources {
        let label = match source {
            Source::Room { room, .. } => resolve_room_label(None, room),
            Source::Account { .. } => "account data".to_string(),
            Source::RoomDirectory { .. } => "the room directory".to_string(),
            Source::UnknownPrivate => "stored data".to_string(),
        };
        if !labels.contains(&label) {
            labels.push(label);
        }
        for recipient in &recipients {
            let already = a2app_core::information_flow::sharing_allows_for_reader(source, recipient, context)
                .unwrap_or(false);
            items.push(PlanItem {
                id: format!("flow:{}:{}", serde_json::to_string(source).unwrap_or_default(),
                    serde_json::to_string(recipient).unwrap_or_default()),
                origin: ItemOrigin::Requested,
                action: PlanAction::Flow { source: source.clone(), recipient: recipient.clone() },
                state: if already { ItemState::AlreadyAllowed } else { ItemState::NeedsGrant },
                why: None,
                risk: Risk::High,
            });
        }
    }
    let explanation = format!(
        "Earlier in this conversation the assistant read {}. Allow it to keep using that information for this turn.",
        labels.join(", ")
    );
    let mut plan = TaskPlan {
        task_id,
        subject: agent_subject(room_id.as_str()),
        context: context.clone(),
        epoch: a2app_core::information_flow::context_epoch(context).unwrap_or(0),
        title: "Keep using earlier context".to_string(),
        explanation,
        items,
        flow_dependencies: BTreeMap::new(),
        plan_hash: [0; 32],
        needs_fingerprint: [0; 32],
    };
    plan.seal();
    plan
}

/// Holds `texts` behind one carried-over prompt when the agent's live label
/// has sources the provider or final reply recipients may no longer receive. Returns true when the
/// texts were queued (a prompt already pending or freshly raised); false lets
/// the caller deliver them.
#[cfg(unix)]
fn raise_carried_permission_prompt(room_id: &OwnedRoomId, texts: &[(OwnedEventId, String)]) -> bool {
    let Ok(context) = super::information_flow::agent_context(room_id.as_str()) else { return false };
    let (provider, homeserver, task_id) = with_a2app(|state| {
        let provider = a2app_agent::model_transport::current_recipient(&state.agent_prefs)
            .ok()
            .map(|recipient| Recipient::ModelProvider(recipient.id));
        let homeserver = crate::sliding_sync::get_client()
            .and_then(|client| Recipient::network_origin(client.homeserver().as_str()).ok());
        (provider, homeserver, state.next_task_id.saturating_add(1))
    })
    .unwrap_or((None, None, 0));
    let Some(provider) = provider else { return false };
    hold_carried_permission_prompt(room_id, texts, &context, &provider, homeserver.as_ref(), task_id)
}

#[cfg(unix)]
fn hold_carried_permission_prompt(
    room_id: &OwnedRoomId,
    texts: &[(OwnedEventId, String)],
    context: &a2app_core::information_flow::ContextId,
    provider: &Recipient,
    homeserver: Option<&Recipient>,
    task_id: u64,
) -> bool {
    let texts = &texts[texts.len().saturating_sub(super::ai::session::MAX_QUEUED_PROMPTS)..];
    let own_room = Recipient::MatrixRoom { account: context.account().into(), room: room_id.to_string() };
    let mut recipients = vec![provider, &own_room];
    if let Some(homeserver) = homeserver { recipients.push(homeserver); }
    let mut carried = BTreeSet::new();
    for recipient in recipients {
        match a2app_core::information_flow::carried_over_sources(context, recipient) {
            Ok(sources) => carried.extend(sources),
            Err(_) => return false,
        }
    }
    if carried.is_empty() { return false; }
    // A prompt already waiting (or showing) for this room absorbs the text
    // instead of raising a second one.
    let appended = with_a2app(|state| update_pending_carried_texts(state, room_id, texts))
    .unwrap_or(false);
    if appended {
        return true;
    }
    let plan = build_carried_task_plan(room_id, context, &carried, provider, homeserver, task_id);
    // A decline is remembered for the turn by the plan's needs, exactly as an
    // agent-requested plan is: do not raise the same carried prompt twice.
    let dismissed = with_a2app(|state| {
        task_plan_is_dismissed(&state.dismissed_task_plans, room_id.as_str(), &plan.need_keys())
    })
    .unwrap_or(false);
    if dismissed {
        return false;
    }
    with_a2app(|state| {
        state.next_task_id = task_id;
        state.task_prompts.push_back(TaskPrompt {
            room_id: room_id.clone(),
            plan,
            resume: TaskResume::UserPrompt { texts: texts.to_vec() },
        });
    });
    // `forward_ai_room_texts` is called from the room screen while the runtime
    // pass that shows queued prompts may already have run; wake it so the
    // carried-over modal appears without waiting for another event.
    makepad_widgets::SignalToUI::set_ui_signal();
    true
}

#[cfg(unix)]
fn update_pending_carried_texts(
    state: &mut A2AppState,
    room_id: &OwnedRoomId,
    texts: &[(OwnedEventId, String)],
) -> bool {
    let existing = state.active_task.as_mut()
        .filter(|prompt| &prompt.room_id == room_id && matches!(&prompt.resume, TaskResume::UserPrompt { .. }))
        .or_else(|| state.task_prompts.iter_mut().find(|prompt|
            &prompt.room_id == room_id && matches!(&prompt.resume, TaskResume::UserPrompt { .. })));
    let Some(prompt) = existing else { return false };
    let TaskResume::UserPrompt { texts: held } = &mut prompt.resume else { return false };
    // The host retains ownership while the modal is open. Its bounded,
    // deduplicated snapshot also removes asks evicted from the queue.
    *held = texts[texts.len().saturating_sub(super::ai::session::MAX_QUEUED_PROMPTS)..].to_vec();
    true
}

/// Whether a parked permission prompt is holding a tool call for `room_id`.
#[cfg(unix)]
fn prompt_parks_room_tool(prompt: &PermissionPrompt, room_id: &OwnedRoomId) -> bool {
    prompt.parked.iter().any(|p| {
        matches!(p, ParkedRequest::AiTool { room_id: r, .. } if r == room_id)
            || matches!(p, ParkedRequest::NetworkAccess { room_id: r, .. } if r == room_id)
    })
}

/// Expires app-tool invocations the app never answered and withdraws tools
/// whose owning instance is gone. Called once per event pass before jobs are
/// drained, so a dead or slow app never leaves the model waiting past the
/// timeout and a quit app's tools never linger in the model's list.
#[cfg(unix)]
fn sweep_app_tools() {
    // A live tool belongs to a live isolate. Quit/uninstalled apps drop
    // theirs, and any call parked on the tool is answered at once.
    let dead: Vec<String> = with_a2app(|state| {
        state
            .app_tools
            .iter()
            .filter(|(_, reg)| instances::key_of_heap(reg.heap_key)
                .is_none_or(|(app, room)| app != reg.app_id || room.as_ref() != Some(&reg.room_id)))
            .map(|(name, _)| name.clone())
            .collect()
    })
    .unwrap_or_default();
    let mut failed_rooms = Vec::new();
    for name in dead {
        with_a2app(|state| {
            if let Some(reg) = state.app_tools.remove(&name) {
                if transfer_app_tool_provenance(state, &reg.app_id, reg.heap_key, &reg.room_id).is_err() {
                    state.ai_sessions.remove(&reg.room_id);
                    failed_rooms.push(reg.room_id.clone());
                }
                if let Some(session) = state.ai_sessions.get(&reg.room_id) {
                    session.unregister_miniapp_tool(&name);
                }
            }
            let ids: Vec<u64> = state
                .app_tool_calls
                .iter()
                .filter(|(_, p)| p.full_name == name)
                .map(|(id, _)| *id)
                .collect();
            for id in ids {
                if let Some(p) = state.app_tool_calls.remove(&id) {
                    let _ = p.answer.send(Err(format!(
                        "the mini-app tool `{name}` stopped before it answered"
                    )));
                }
            }
        });
    }
    for room in failed_rooms { stop_ai_session(&room); }
    // Timeouts: an app that got the hook but never replied must not hold the
    // model's serve thread past the bound.
    let expired: Vec<u64> = with_a2app(|state| {
        let now = Instant::now();
        state
            .app_tool_calls
            .iter()
            .filter(|(_, p)| now.duration_since(p.since) >= APP_TOOL_TIMEOUT)
            .map(|(id, _)| *id)
            .collect()
    })
    .unwrap_or_default();
    for id in expired {
        if let Some(p) = with_a2app(|state| state.app_tool_calls.remove(&id)).flatten() {
            let protected = with_a2app(|state| transfer_app_tool_provenance(state, &p.app_id, p.heap_key, &p.room_id))
                .unwrap_or_else(|| Err("Mini-app state is unavailable.".into()));
            if let Err(error) = protected {
                stop_ai_session(&p.room_id);
                let _ = p.answer.send(Err(error));
                continue;
            }
            let reason = format!("the mini-app did not answer `{}` in time", p.full_name);
            note_ai_tool_call(&p.room_id, &p.display_name, false, &reason);
            let _ = p.answer.send(Err(reason));
        }
    }
}

/// Whether `room_id`'s agent session already has a tool call in flight: a
/// granted read, a cross-room post, a generation, an app-tool invocation
/// awaiting its isolate, or a call parked behind the permission prompt. The
/// runtime won't dispatch the next tool call until the current one is
/// answered, so a session's tools run serially.
#[cfg(unix)]
fn session_has_inflight_tool(state: &A2AppState, room_id: &OwnedRoomId) -> bool {
    state.ai_rooms.get(room_id).is_some_and(|info| info.final_writes_pending_revoke)
        || state.ai_reads.values().any(|(r, _, _)| r == room_id)
        || state.ai_posts.values().any(|(r, _)| r == room_id)
        || state.app_tool_calls.values().any(|p| &p.room_id == room_id)
        || state
            .ai_sessions
            .get(room_id)
            .is_some_and(|session| session.is_generating())
        || state
            .active_prompt
            .as_ref()
            .is_some_and(|p| prompt_parks_room_tool(p, room_id))
        || state.active_prompt_batch.iter().any(|p| prompt_parks_room_tool(p, room_id))
        || state.prompts.iter().any(|p| prompt_parks_room_tool(p, room_id))
        || state.active_task.as_ref().is_some_and(|p| &p.room_id == room_id)
        || state.task_prompts.iter().any(|p| &p.room_id == room_id)
        || state.active_exact_review.as_ref().is_some_and(|p| &p.room_id == room_id)
        || state.exact_reviews.iter().any(|p| &p.room_id == room_id)
}

/// Drives every room's AI session for one event pass. First the tool calls
/// that arrived on the sessions' serve threads are executed (the real work:
/// posting to the room, starting the generation pipeline); then each agent
/// is advanced and its replies/errors/deaths are acted on.
#[cfg(unix)]
fn advance_ai_sessions(cx: &mut Cx, ui: &WidgetRef) {
    // 0. Expire unanswered app-tool calls and withdraw tools whose isolate is
    //    gone, so a dead or slow mini-app never strands the model or leaves a
    //    stale tool in the model's list.
    sweep_app_tools();

    // 1. Tool calls waiting on the UI thread. One per session per pass, and
    //    only when that session has no tool already in flight, so an agent's
    //    calls execute serially instead of all starting at once.
    let jobs: Vec<(OwnedRoomId, SessionJob)> = with_a2app(|state| {
        let all_rooms: Vec<OwnedRoomId> = state.ai_sessions.keys().cloned().collect();
        let ready_rooms: Vec<OwnedRoomId> = all_rooms
            .into_iter()
            .filter(|room_id| !session_has_inflight_tool(state, room_id))
            .collect();
        let mut out = Vec::new();
        for room_id in ready_rooms {
            if let Some(job) = state.ai_sessions.get_mut(&room_id).and_then(|s| s.try_recv_job()) {
                out.push((room_id, job));
            }
        }
        out
    }).unwrap_or_default();
    for (room_id, job) in jobs {
        execute_session_job(cx, ui, &room_id, job);
    }

    // 2. Agent events since the last pass.
    let mut to_post: Vec<(OwnedRoomId, String)> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut deaths: Vec<(OwnedRoomId, String)> = Vec::new();
    {
        let updates: Vec<(OwnedRoomId, Vec<SessionUpdate>)> = with_a2app(|state| {
            state
                .ai_sessions
                .iter_mut()
                .map(|(room_id, session)| {
                    let updates = session.advance();
                    (room_id.clone(), updates)
                })
                .filter(|(_, updates)| !updates.is_empty())
                .collect()
        }).unwrap_or_default();
        for (room_id, updates) in updates {
            for update in updates {
                match update {
                    SessionUpdate::Ready => log!("AI Rooms: room {room_id}'s agent session is ready."),
                    SessionUpdate::Thinking => {
                        // First reasoning chunk of a turn: the turn card is
                        // opened (or updated) with a `thinking` marker, so a
                        // turn that thinks before it calls a tool shows a warm
                        // card from its very first activity instead of a
                        // separate one-line row.
                        log!("AI Rooms: room {room_id}'s agent started thinking.");
                        note_ai_turn_thinking(&room_id);
                    }
                    SessionUpdate::ToolCallStarted { name } => {
                        // Every tool call becomes its own `ai_tool_call`
                        // state row immediately, so the room sees the agent
                        // pick the tool while it is still running it.
                        log!("AI Rooms: room {room_id}'s agent started tool {name}.");
                        on_ai_tool_call_started(&room_id, &name);
                    }
                    SessionUpdate::ToolCallFinished { name, ok, summary } => {
                        // The agent's OWN tool (octos's web_search/web_fetch/
                        // browser) finished; close its live row. A Robrix
                        // tool's row was already rewritten Done by the host,
                        // so this finds no running row and does nothing; no
                        // receipt chip either way (the result never reached
                        // the model through Robrix).
                        let summary: String = summary.chars().collect();
                        log!(
                            "AI Rooms: room {room_id}'s agent tool {name} finished (ok: {ok}{}).",
                            if summary.is_empty() { String::new() } else { format!(": {summary}") }
                        );
                        finish_ai_tool_call(&room_id, &name, ok, &summary);
                    }
                    SessionUpdate::Reply { text } => {
                        log!("AI Rooms: room {room_id}'s agent finished a turn ({} chars).", text.len());
                        // A turn whose `send_message` tool already posted to
                        // the room ends with a redundant confirmation from the
                        // agent; drop it so the tool's message is the turn's
                        // one `ai_reply` (octos still required the non-empty
                        // text to accept the end-of-turn response).
                        let posted_by_tool = with_a2app(|state| {
                            state
                                .ai_rooms
                                .get_mut(&room_id)
                                .map(|info| {
                                    let posted = std::mem::take(&mut info.posted_by_tool_this_turn);
                                    if posted {
                                        // Receipts recorded after the tool's
                                        // post would ride the NEXT turn's card.
                                        info.pending_tool_calls.clear();
                                    }
                                    posted
                                })
                                .unwrap_or(false)
                        }).unwrap_or(false);
                        if posted_by_tool {
                            log!("AI Rooms: room {room_id}'s turn already posted via the send_message tool; dropping its {} trailing text.", text.chars().count());
                            // The only final write left is the `Done` snapshot;
                            // its result releases the turn's grants.
                            close_active_turn(&room_id, false);
                            settle_closed_turn(&room_id);
                        } else {
                            to_post.push((room_id.clone(), text));
                            // The reply and the Done snapshot are still being
                            // written; keep this turn's grants until both land
                            // (the reply is queued below, then settled).
                            close_active_turn(&room_id, false);
                        }
                    }
                    SessionUpdate::TurnEnded => {
                        // The turn ended without a reply (cancelled via Escape,
                        // or empty output): settle the turn card and drop this
                        // turn's receipts so nothing leaks into the next turn.
                        // No error row and no popup — the user asked for this.
                        with_a2app(|state| {
                            if let Some(info) = state.ai_rooms.get_mut(&room_id) {
                                info.posted_by_tool_this_turn = false;
                                info.pending_tool_calls.clear();
                            }
                        });
                        close_active_turn(&room_id, false);
                        settle_closed_turn(&room_id);
                        ui.redraw(cx);
                    }
                    SessionUpdate::Error(msg) => {
                        // The turn is over without a reply; clear the tool-post
                        // flag (and this turn's receipts) so the next turn's
                        // reply is not wrongly dropped or attributed, settle
                        // the turn card, and post an error row into the chat so
                        // the failure is part of the room's transcript.
                        with_a2app(|state| {
                            if let Some(info) = state.ai_rooms.get_mut(&room_id) {
                                info.posted_by_tool_this_turn = false;
                                info.pending_tool_calls.clear();
                            }
                        });
                        close_active_turn(&room_id, false);
                        log!("AI Rooms: room {room_id}'s agent session reported an error: {msg}");
                        post_ai_activity(&room_id, AiActivityKind::Error, Some(&msg));
                        settle_closed_turn(&room_id);
                        errors.push(msg)
                    }
                    SessionUpdate::Gone(msg) => {
                        log!("AI Rooms: room {room_id}'s agent session is GONE: {msg}");
                        // The session is dead, but its final Stopped row and
                        // Done snapshot still need the turn's grants: keep them
                        // until those writes land, then revoke and retire.
                        close_active_turn(&room_id, false);
                        post_ai_activity(&room_id, AiActivityKind::Stopped, Some(&msg));
                        settle_closed_turn(&room_id);
                        deaths.push((room_id.clone(), msg))
                    }
                }
            }
        }
    }
    for (room_id, text) in to_post {
        post_ai_reply(&room_id, text, None);
        // The natural reply registered its final-write token; settle now that
        // it is queued before any subsequent member turn is released.
        settle_closed_turn(&room_id);
    }
    for msg in errors {
        log!("AI Rooms: showing session error popup: {msg}");
        enqueue_popup_notification(format!("AI session error: {msg}"), PopupKind::Error, Some(6.0));
    }
    for (room_id, msg) in deaths {
        enqueue_popup_notification(
            format!("The AI agent in this room stopped: {msg}"),
            PopupKind::Warning, Some(8.0),
        );
        // A session that never finished its handshake is a failed start:
        // give it the start cooldown, or every message would respawn one.
        with_a2app(|state| {
            let never_ready = state.ai_sessions.get(&room_id).is_some_and(|s| !s.is_ready());
            if never_ready && let Some(info) = state.ai_rooms.get_mut(&room_id) {
                info.start_failed_at = Some(Instant::now());
            }
        });
        // A dead session is a (re)start boundary: its one-time grants,
        // in-flight reads/posts, parked prompts and turn state all go, exactly
        // as the panel's power-off does it. Its final Stopped row and Done
        // snapshot, though, still need the session's flow context, so the
        // teardown waits for them when they are outstanding.
        refuse_room_prompts(cx, ui, &room_id);
        retire_ai_session(&room_id);
        ui.redraw(cx);
    }

    // Final writes must settle before another member turn is released. The
    // fresh carried-source check runs after rollback, while host jobs remain
    // gated during the drain of the preceding turn's writes.
    flush_pending_ai_turns(cx);
    drain_pending_member_prompts();

    // 3. Keep each room's busy/queued status row honest between timeline
    //    updates: a turn ending with no reply, the queue flushing the next
    //    prompt, or the agent finishing its startup all change it without a
    //    new timeline event. Redraw rooms whose row would change, once per
    //    pass (the row's text itself is populated at draw time).
    let status_changed: Vec<OwnedRoomId> = with_a2app(|state| {
        let mut changed = Vec::new();
        for (room_id, session) in state.ai_sessions.iter() {
            let Some(info) = state.ai_rooms.get_mut(room_id) else { continue };
            let (busy, queued) = (session.is_busy(), session.queued_len() + info.pending_member_prompts.len());
            if info.status_busy != busy || info.status_queued != queued {
                info.status_busy = busy;
                info.status_queued = queued;
                changed.push(room_id.clone());
            }
        }
        changed
    })
    .unwrap_or_default();
    if !status_changed.is_empty() {
        ui.redraw(cx);
    }

}

/// Whether an AI room's profile declares one permission group: it does if any
/// offered capability sits in it (mirrors `MiniAppManifest::normalize`: a
/// declared capability implies its group). The profile itself is
/// [`AI_ROOM_SESSION_CAP_IDS`] in `ai::tools`, the single source for both the
/// tool list and this gate.
#[cfg(unix)]
fn ai_room_declares_perm(perm: Permission) -> bool {
    AI_ROOM_SESSION_CAP_IDS.iter().any(|id| {
        a2app_core::capabilities::by_id(id)
            .and_then(|c| c.group)
            .is_some_and(|g| g == perm)
    })
}

/// Whether an AI room's profile declares one capability.
#[cfg(unix)]
fn ai_room_declares_cap(cap: &a2app_core::capabilities::Capability) -> bool {
    AI_ROOM_SESSION_CAP_IDS.contains(&cap.id)
}

/// One app tool's (`list_apps`, `launch_app`) verdict against its room's
/// subject, with the same shape as a read's gate: a granted verdict records
/// the group use so the room's panel shows it, exactly as a granted read
/// does. The caller still owns the answer and decides whether to run, refuse,
/// or park behind the prompt.
#[cfg(unix)]
fn ai_app_tool_verdict(
    room_id: &OwnedRoomId,
    cap: &a2app_core::capabilities::Capability,
) -> Effective {
    let subject = agent_subject(room_id.as_str());
    let verdict =
        with_a2app(|state| ai_capability_verdict(state, room_id, cap)).unwrap_or(Effective::Denied);
    if verdict == Effective::Granted {
        if let Some(group) = cap.group {
            with_a2app(|state| {
                state.permissions.record_access(&subject, group, versions::now_unix());
                state.perms_dirty = true;
            });
        }
    }
    verdict
}

/// The session's verdict for one catalog capability against its room subject
/// — the shared gate the mini-app broker mirrors, so a room's AI and an app
/// can never be decided differently for the same capability.
#[cfg(unix)]
fn ai_capability_verdict(
    state: &A2AppState,
    room_id: &OwnedRoomId,
    cap: &a2app_core::capabilities::Capability,
) -> Effective {
    let subject = agent_subject(room_id.as_str());
    state.permissions.effective_capability_for_in_context(
        &subject, ai_room_declares_perm, ai_room_declares_cap, cap,
        PermissionContext { origin_room: Some(room_id.as_str()), target_room: Some(room_id.as_str()) },
    )
}

/// Executes one capability-gated attached-room read for a session. Decides
/// against the room's permission subject (`agent_subject(room_id)`) exactly as
/// the broker decides for a mini-app: a granted call records its use and is
/// fetched on the async worker (the tool call waits, answered when the result
/// lands); a refused call answers with an error the model can read and act on;
/// a first use parks the call behind the permission prompt until the user
/// answers.
#[cfg(unix)]
fn ai_read_authorization(
    room_id: &OwnedRoomId,
    kind: &ReadToolKind,
    context: &a2app_core::information_flow::ContextId,
    permissions: &PermissionStore,
) -> Option<matrix::policy::MatrixAuthorization> {
    let cap = kind.capability()?;
    let mut auth = matrix::policy::MatrixAuthorization::new(
        &agent_subject(room_id.as_str()), cap.id, Some(room_id.as_str()), permissions,
    ).with_flow(context.clone());
    auth.target_room = Some(match kind {
        ReadToolKind::OtherRoom { room, .. } => room.clone(),
        ReadToolKind::SpaceInfo { space } | ReadToolKind::SpaceRooms { space } => space.clone(),
        _ => room_id.to_string(),
    });
    if let ReadToolKind::SpaceRooms { space } = kind {
        // The hierarchy service reviews the immutable query before its first
        // page, and later pages retain this capability and scope proof.
        auth = auth.with_payload(serde_json::json!({ "space_id": space }));
    } else if let ReadToolKind::Older { before, limit } = kind {
        auth = auth.with_payload(serde_json::json!({ "room_id": room_id, "before": before, "limit": limit }));
    }
    Some(auth)
}

#[cfg(unix)]
fn run_ai_read_tool(
    cx: &mut Cx,
    ui: &WidgetRef,
    room_id: &OwnedRoomId,
    kind: ReadToolKind,
    answer: Sender<Result<String, String>>,
) {
    let target = match &kind {
        ReadToolKind::OtherRoom { room, .. } => room.as_str(),
        ReadToolKind::SpaceInfo { space } | ReadToolKind::SpaceRooms { space } => space.as_str(),
        _ => room_id.as_str(),
    };
    let flow_context = match super::information_flow::prepare_agent(room_id.as_str()).and_then(|context| {
        // Reading cached data sends nothing to room members. Any remote
        // fallback checks the actual homeserver immediately before the SDK call.
        // The directory tools carry one `RoomDirectory` source (and its
        // untrusted influence) instead of one source per listed room, so one
        // provider rule covers the whole directory and other recipients do not
        // inherit every listed room. Everything else labels its target room.
        if is_directory_kind(&kind) {
            super::information_flow::record_directory_response(&context)?;
        } else {
            let source = super::information_flow::room_source(&context, target);
            a2app_core::information_flow::add_sources(&context, [source])?;
            a2app_core::information_flow::add_influences(&context, [a2app_core::information_flow::Influence::RoomContent {
                account: super::information_flow::context_account(&context).into(), room: target.into(),
            }])?;
        }
        Ok(context)
    }) {
        Ok(context) => context,
        Err(error) => { let _ = answer.send(Err(super::information_flow::with_task_reask_hint(error))); return; }
    };
    if !room_policy_allows(target, RoomAccess::Read) {
        let _ = answer.send(Err("Reading this room is blocked in Mini Apps permissions.".into()));
        return;
    }
    // `read_room_memory` is ungated room plumbing — the agent recalling its
    // OWN past replies — not a catalog capability, so it never floods, asks,
    // records, or gets refused. Park it on the same id map the granted reads
    // use and fetch it on the worker like any other read.
    if let ReadToolKind::Memory { .. } = &kind {
        let id = NEXT_AI_TOOL_ID.fetch_add(1, Ordering::Relaxed);
        with_a2app(|state| {
            state.ai_reads.insert(id, (room_id.clone(), kind.clone(), answer));
        });
        submit_async_request(MatrixRequest::AiRoom(AiRoomRequest::ToolRead {
            id,
            room_id: room_id.clone(),
            tool: kind,
            authorization: None,
            flow_epoch: a2app_core::information_flow::context_epoch(&flow_context).unwrap_or(0),
            flow_context,
        }));
        return;
    }
    let Some(cap) = kind.capability() else {
        let _ = answer.send(Err("this tool is no longer offered".to_string()));
        return;
    };
    // Flood guard first: refuse a read loop before it spends worker time. A
    // fresh window starts once the last one ages out.
    let over_budget = with_a2app(|state| {
        state.ai_rooms.get_mut(room_id).map(|info| {
            let now = Instant::now();
            if info
                .read_window_started
                .map(|w| now.duration_since(w) > AI_READ_WINDOW)
                .unwrap_or(true)
            {
                info.read_window_started = Some(now);
                info.read_burst = 0;
            }
            info.read_burst += 1;
            info.read_burst > AI_READ_BUDGET
        }).unwrap_or(false)
    }).unwrap_or(false);
    if over_budget {
        let text = String::from(
            "This room's AI is making too many read calls at once; stop and wait a few seconds.",
        );
        note_ai_tool_call(room_id, read_tool_name(&kind), false, &text);
        let _ = answer.send(Err(text));
        return;
    }
    let subject = agent_subject(room_id.as_str());
    // Cross-room reads are granted PER ROOM, exactly like cross-room posts:
    // the `matrix.rooms.messages.read` group is the kill switch and the prompt
    // unit, but a group `Granted` alone must not unlock every room the agent
    // names. Only the agent's own-room reads keep the ordinary group gate.
    let target_room = match &kind {
        ReadToolKind::OtherRoom { room, .. }
        | ReadToolKind::SpaceInfo { space: room }
        | ReadToolKind::SpaceRooms { space: room } => match OwnedRoomId::try_from(room.as_str()) {
            Ok(target) => Some(target),
            Err(_) => {
                let err = format!("`{room}` is not a valid matrix room id");
                note_ai_tool_call(room_id, read_tool_name(&kind), false, &err);
                let _ = answer.send(Err(err));
                return;
            }
        },
        _ => None,
    };
    let verdict = with_a2app(|state| {
        let target = target_room.as_deref().map(|r| r.as_str()).unwrap_or(room_id.as_str());
        let context = PermissionContext { origin_room: Some(room_id.as_str()), target_room: Some(target) };
        let verdict = if services::is_room_collection(cap) {
            state.permissions.effective_collection_capability_for_in_context(
                &subject, ai_room_declares_perm, ai_room_declares_cap, cap, context,
            )
        } else {
            state.permissions.effective_capability_for_in_context(
                &subject, ai_room_declares_perm, ai_room_declares_cap, cap, context,
            )
        };
        if verdict == Effective::NeedsPrompt && target_room.is_some()
            && (state.permissions.is_room_read_allowed(&subject, target)
                || cap.group.is_some_and(|group| state.once_rooms.contains(&(subject.clone(), group, target.to_string()))))
        {
            Effective::Granted
        } else { verdict }
    }).unwrap_or(Effective::Denied);
    log!(
        "AI Rooms: room {}'s {} tool verdict: {:?}",
        room_id,
        read_tool_name(&kind),
        verdict
    );
    match verdict {
        Effective::Granted => {
            // A granted read records that the session actually used it — the
            // same "Used" record a mini-app's granted call leaves, so the
            // room's AI panel can show what it has been doing. (A cross-room
            // read's allowance is the per-room entry the prompt wrote, not a
            // blanket group grant.)
            if let Some(group) = cap.group {
                with_a2app(|state| {
                    state.permissions.record_access(&subject, group, versions::now_unix());
                    state.perms_dirty = true;
                });
            }
            let id = NEXT_AI_TOOL_ID.fetch_add(1, Ordering::Relaxed);
            with_a2app(|state| {
                state.ai_reads.insert(id, (room_id.clone(), kind.clone(), answer));
            });
            // Collection consent starts the read; its authorization still
            // filters every directory row and rechecks current user consent.
            let authorization = with_a2app(|state| {
                ai_read_authorization(room_id, &kind, &flow_context, &state.permissions)
            }).flatten();
            submit_async_request(MatrixRequest::AiRoom(AiRoomRequest::ToolRead {
                id,
                room_id: room_id.clone(),
                tool: kind,
                authorization,
                flow_epoch: a2app_core::information_flow::context_epoch(&flow_context).unwrap_or(0),
                flow_context,
            }));
        }
        Effective::Denied | Effective::Undeclared => {
            let text = match cap.group {
                Some(group) => ai_tool_refused_text(group),
                None => String::from("The user has not allowed the AI in this room to do that."),
            };
            note_ai_tool_call(room_id, read_tool_name(&kind), false, &text);
            let _ = answer.send(Err(text));
        }
        Effective::NeedsPrompt => {
            // First use of a runtime-tier group: park the call and ask, exactly
            // as a mini-app's first use does. The tool call is left unanswered
            // — its serve thread blocks until the prompt resolves it.
            let Some(group) = cap.group else { return };
            queue_permission_prompt(
                cx,
                ui,
                subject,
                group,
                ParkedRequest::AiTool {
                    room_id: room_id.clone(),
                    job: SessionJob::ReadTool { kind, answer },
                },
                None,
            );
        }
    }
}

/// Executes the `post_room_message` tool for a session: posts the agent's
/// text into ANOTHER joined room as an `m.notice` message, gated PER ROOM.
/// Unlike a group-granted read, an allowance covers exactly the target
/// room: the user is asked the first time this agent posts into each room it
/// names, and the room joins the subject's send allowlist on Allow. A group
/// `Denied` on `matrix.rooms.message.send` is the kill switch; a group
/// `Granted` alone never unlocks a room.
#[cfg(unix)]
fn run_ai_room_post(
    cx: &mut Cx,
    ui: &WidgetRef,
    session_room: &OwnedRoomId,
    target: String,
    text: String,
    answer: Sender<Result<String, String>>,
) {
    // The tool validated text already; refuse a room that cannot be posted
    // into as a malformed call.
    let Ok(target_room) = OwnedRoomId::try_from(target.as_str()) else {
        let err = format!("`{target}` is not a valid matrix room id");
        note_ai_tool_call(session_room, "post_room_message", false, &err);
        let _ = answer.send(Err(err));
        return;
    };
    let Some(cap) = a2app_core::capabilities::by_id("matrix.rooms.message.send") else {
        let _ = answer.send(Err("posting into other rooms is no longer offered".to_string()));
        return;
    };
    let Some(group) = cap.group else {
        let _ = answer.send(Err("posting into other rooms is no longer offered".to_string()));
        return;
    };
    let subject = agent_subject(session_room.as_str());
    // The per-room decision lives in the store: a durable group Denied is the
    // kill switch; otherwise the allowlist decides room by room.
    let verdict = with_a2app(|state| {
        let verdict = state.permissions.effective_capability_for_in_context(
            &subject, ai_room_declares_perm, ai_room_declares_cap, cap,
            PermissionContext { origin_room: Some(session_room.as_str()), target_room: Some(&target) },
        );
        if verdict == Effective::NeedsPrompt
            && (state.permissions.is_room_send_allowed(&subject, &target)
                || state.once_rooms.contains(&(subject.clone(), group, target.clone())))
        { Effective::Granted } else { verdict }
    }).unwrap_or(Effective::Denied);
    let job = SessionJob::PostRoomMessage {
        room_id: target,
        text,
        answer,
    };
    match verdict {
        Effective::Granted => {
            // Allowed for this room: record the group use and post on the
            // async worker (the tool call waits, answered when the write
            // lands).
            with_a2app(|state| {
                state.permissions.record_access(&subject, group, versions::now_unix());
                state.perms_dirty = true;
            });
            post_ai_room_message(session_room, &target_room, job);
        }
        Effective::Denied | Effective::Undeclared => {
            let text = ai_tool_refused_text(group);
            note_ai_tool_call(session_room, "post_room_message", false, &text);
            answer_session_job(job, Err(text));
        }
        Effective::NeedsPrompt => {
            // First time into this room: park the call behind the prompt.
            // The room's name shows on the prompt (see `ai_prompt_action` /
            // `ai_prompt_reason`); an Allow grants exactly this room.
            queue_permission_prompt(
                cx,
                ui,
                subject,
                group,
                ParkedRequest::AiTool {
                    room_id: session_room.clone(),
                    job,
                },
                None,
            );
        }
    }
}

/// Parks a granted cross-room post on the worker and remembers the call so
/// [`AiRoomAction::PostToRoomResult`] can answer it (mirrors the granted read
/// path).
#[cfg(unix)]
fn post_ai_room_message(
    session_room: &OwnedRoomId,
    target: &OwnedRoomId,
    job: SessionJob,
) {
    let SessionJob::PostRoomMessage { room_id, text, answer } = job else { return };
    let flow_context = match super::information_flow::agent_context(session_room.as_str()).and_then(|context| {
        super::information_flow::ensure_room_output(&context, target.as_str())?;
        Ok(context)
    }) {
        Ok(context) => context,
        Err(error) => { let _ = answer.send(Err(super::information_flow::with_task_reask_hint(error))); return; }
    };
    let id = NEXT_AI_TOOL_ID.fetch_add(1, Ordering::Relaxed);
    with_a2app(|state| {
        state.ai_posts.insert(id, (session_room.clone(), answer));
    });
    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    submit_async_request(MatrixRequest::AiRoom(AiRoomRequest::PostToRoom {
        id,
        target: OwnedRoomId::try_from(room_id).unwrap_or_else(|_| target.clone()),
        flow_epoch: a2app_core::information_flow::context_epoch(&flow_context).unwrap_or(0),
        flow_context,
        content: AiReplyContent {
            v: 1,
            text: text.clone(),
            formatted: crate::a2app::ai_room_events::agent_reply_formatted_html(&text),
            tool_calls: Vec::new(),
            model: None,
            created_at,
            in_reply_to: None,
        },
    }));
}

/// Executes the `list_apps` tool: the installed apps this room's agent may
/// launch — apps compatible with this room — as JSON the
/// model reads and picks a `launch_app` id from. Gated like a read against
/// the room's subject (`apps.list`), so a first use parks the call behind the
/// permission prompt.
#[cfg(unix)]
fn run_ai_list_apps(
    cx: &mut Cx,
    ui: &WidgetRef,
    room_id: &OwnedRoomId,
    answer: Sender<Result<String, String>>,
) {
    let Some(cap) = a2app_core::capabilities::by_id(LIST_APPS_CAP_ID) else {
        let _ = answer.send(Err("this tool is no longer offered".to_string()));
        return;
    };
    match ai_app_tool_verdict(room_id, cap) {
        Effective::Granted => {
            let apps = with_a2app(|state| {
                state
                    .registry
                    .iter()
                    .filter(|m| m.can_run_in_context(room_id.as_str(), false))
                    .map(|m| {
                        let mut entry = serde_json::json!({
                            "id": m.id,
                            "name": m.name,
                            "description": m.description,
                            "scope": match &m.scope {
                                A2AppScope::Room { .. } => "room",
                                A2AppScope::Account => "account",
                            },
                            "builtin": m.builtin,
                            "running": instances::is_running(&m.id),
                        });
                        if let A2AppScope::Room { room_id } = &m.scope {
                            entry["room_id"] = serde_json::json!(room_id);
                        }
                        entry
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
            let value = serde_json::json!({ "apps": apps });
            let record = super::information_flow::agent_context(room_id.as_str()).and_then(|context| {
                // `list_apps` exposes names and descriptions, not code, so it
                // does not join the apps' code labels: one app with unknown
                // provenance must not wedge the listing. Paths that actually
                // read an app's code join it fail-closed (`join_manifest_code`).
                a2app_core::information_flow::add_sources(&context, [super::information_flow::account_source(&context)])?;
                super::information_flow::record_response(&context, &value)
            });
            if let Err(error) = record { let _ = answer.send(Err(super::information_flow::with_task_reask_hint(error))); return; }
            let text = value.to_string();
            note_ai_tool_call(room_id, "list_apps", true, "");
            let _ = answer.send(Ok(text));
        }
        Effective::Denied | Effective::Undeclared => {
            let text = match cap.group {
                Some(group) => ai_tool_refused_text(group),
                None => String::from("The user has not allowed the AI in this room to do that."),
            };
            note_ai_tool_call(room_id, "list_apps", false, &text);
            let _ = answer.send(Err(text));
        }
        Effective::NeedsPrompt => {
            let Some(group) = cap.group else {
                let _ = answer.send(Err("this tool is no longer offered".to_string()));
                return;
            };
            queue_permission_prompt(
                cx,
                ui,
                agent_subject(room_id.as_str()),
                group,
                ParkedRequest::AiTool {
                    room_id: room_id.clone(),
                    job: SessionJob::ListApps { answer },
                },
                None,
            );
        }
    }
}

/// Executes the `launch_app` tool for a session: opens an already-installed
/// app in the session's room. This is the run path ONLY — no generation. Like
/// a read, it is gated against the room's subject (`apps.launch`), so a first
/// use parks the call behind the permission prompt. An id that is unknown,
/// requiring another room or a space, or restricted is an error the model reads and can
/// recover from (it can call `list_apps` for valid ids).
#[cfg(unix)]
fn run_ai_launch_app(
    cx: &mut Cx,
    ui: &WidgetRef,
    room_id: &OwnedRoomId,
    app_id: String,
    answer: Sender<Result<String, String>>,
) {
    let Some(cap) = a2app_core::capabilities::by_id(LAUNCH_APP_CAP_ID) else {
        let _ = answer.send(Err("this tool is no longer offered".to_string()));
        return;
    };
    match ai_app_tool_verdict(room_id, cap) {
        Effective::Granted => run_ai_launch_app_granted(cx, room_id, app_id, answer),
        Effective::Denied | Effective::Undeclared => {
            let text = match cap.group {
                Some(group) => ai_tool_refused_text(group),
                None => String::from("The user has not allowed the AI in this room to do that."),
            };
            note_ai_tool_call(room_id, "launch_app", false, &text);
            let _ = answer.send(Err(text));
        }
        Effective::NeedsPrompt => {
            let Some(group) = cap.group else {
                let _ = answer.send(Err("this tool is no longer offered".to_string()));
                return;
            };
            queue_permission_prompt(
                cx,
                ui,
                agent_subject(room_id.as_str()),
                group,
                ParkedRequest::AiTool {
                    room_id: room_id.clone(),
                    job: SessionJob::LaunchApp { app_id, answer },
                },
                None,
            );
        }
    }
}

/// The granted half of [`run_ai_launch_app`]: look the id up and run it, or
/// answer with the reason it cannot run.
#[cfg(unix)]
fn run_ai_launch_app_granted(
    cx: &mut Cx,
    room_id: &OwnedRoomId,
    app_id: String,
    answer: Sender<Result<String, String>>,
) {
    enum Refusal {
        Unknown,
        OtherContext(MiniAppId),
        Restricted(MiniAppId),
    }
    let outcome: Result<MiniAppManifest, Refusal> = with_a2app(|state| {
        let Some(manifest) = state.registry.get(&app_id).cloned() else {
            return Err(Refusal::Unknown);
        };
        if !manifest.can_run_in_context(room_id.as_str(), false) {
            return Err(Refusal::OtherContext(manifest.id));
        }
        if state.permissions.is_restricted(&manifest.id) {
            return Err(Refusal::Restricted(manifest.id));
        }
        Ok(manifest)
    })
    .unwrap_or(Err(Refusal::Unknown));

    match outcome {
        Ok(manifest) => {
            let transfer = super::information_flow::agent_context(room_id.as_str()).and_then(|from| {
                let to = super::information_flow::app_context(&manifest.id, Some(room_id.as_str()))?;
                a2app_core::information_flow::register_context_with_legacy_data(&to, super::information_flow::manifest_has_private_source(&manifest))?;
                a2app_core::information_flow::transfer(&from, &to)?;
                a2app_core::information_flow::transfer(&to, &from)
            });
            if let Err(error) = transfer { let _ = answer.send(Err(super::information_flow::with_task_reask_hint(error))); return; }
            // Dock it into this room exactly as a freshly built app is docked
            // (`resolve_session_generation`), so "launch" lands where the
            // conversation is.
            let Ok(context) = super::information_flow::agent_context(room_id.as_str()) else { return };
            cx.action(A2AppOp::OpenAppFromContext {
                flow_epoch: a2app_core::information_flow::context_epoch(&context).unwrap_or(0),
                context,
                app_id: manifest.id.clone(),
                room_id: room_id.clone(),
            });
            let summary = serde_json::json!({
                "app_id": manifest.id,
                "name": manifest.name,
                "status": "running",
            });
            note_ai_tool_call(room_id, "launch_app", true, "");
            let _ = answer.send(Ok(summary.to_string()));
        }
        Err(refusal) => {
            let text = match refusal {
                Refusal::Unknown => format!(
                    "There is no installed mini-app with id \"{app_id}\". Use list_apps to see the ids that exist."
                ),
                Refusal::OtherContext(id) => format!(
                    "\"{id}\" requires a different room or space, so it can't run here. Use list_apps for the apps available in this room."
                ),
                Refusal::Restricted(id) => format!(
                    "\"{id}\" was stopped for hammering the host with requests; the user has to let it run again from its app info."
                ),
            };
            note_ai_tool_call(room_id, "launch_app", false, &text);
            let _ = answer.send(Err(text));
        }
    }
}

/// Executes the `launch_splash_app` tool for a session: first gated against
/// the room's subject like any other capability (`apps.generate`), then — if
/// granted — run through the same generation machinery the Mini Apps screen
/// uses, with the waiting tool call answered when the build finishes. The
/// build tool is the one capability with real cost (provider tokens), so a
/// first use prompts exactly like a read does. This is CREATE-only: it always
/// runs `Intent::Create`, so the agent can never rewrite an app through it —
/// running an existing app is `launch_app`'s job.
#[cfg(unix)]
fn run_ai_generation(
    cx: &mut Cx,
    ui: &WidgetRef,
    room_id: &OwnedRoomId,
    description: String,
    answer: Sender<Result<String, String>>,
) {
    let Some(cap) = a2app_core::capabilities::by_id("apps.generate") else {
        let _ = answer.send(Err("app generation is no longer offered".to_string()));
        return;
    };
    let subject = agent_subject(room_id.as_str());
    let verdict =
        with_a2app(|state| ai_capability_verdict(state, room_id, cap)).unwrap_or(Effective::Denied);
    match verdict {
        Effective::Granted => {
            if let Some(group) = cap.group {
                with_a2app(|state| {
                    state.permissions.record_access(&subject, group, versions::now_unix());
                    state.perms_dirty = true;
                });
            }
            // Refuse up front when the pipeline cannot start, exactly as the
            // Mini Apps screen would; a tool call must not hang on a build
            // that can never begin.
            let refused = a2app_agent::blocker()
                .map(|b| b.headline())
                .or_else(|| {
                    with_a2app(|state| {
                        state.generation.as_ref().map(|_| {
                            "Another mini-app is already being built in Robrix; \
                             try again in a moment."
                                .to_string()
                        })
                    }).flatten()
                });
            if let Some(reason) = refused {
                note_ai_tool_call(room_id, "launch_splash_app", false, &reason);
                let _ = answer.send(Err(reason));
                return;
            }
            let approval = super::information_flow::agent_context(room_id.as_str()).and_then(|context| {
                a2app_core::information_flow::commit_exact_action_for_activation(&context,
                    a2app_core::information_flow::context_epoch(&context)?, &a2app_core::information_flow::SensitiveAction {
                        kind: "apps.generate".into(), target: room_id.to_string(),
                    }, &serde_json::json!({ "description": description, "room_id": room_id }))
            });
            if let Err(error) = approval { let _ = answer.send(Err(error)); return; }
            // Record where completion must answer, then start the same
            // generation machinery the Mini Apps screen uses. A tool call
            // may not overlap an already-running build, so refuse early is
            // the only concurrency policy (one global pipeline today).
            let mut answer = Some(answer);
            with_a2app(|state| {
                state.ai_generation_room = Some(room_id.clone());
                if let Some(session) = state.ai_sessions.get_mut(room_id) {
                    if let Some(answer) = answer.take() {
                        session.set_generation_answer(answer);
                    }
                }
            });
            start_generation(cx, ui, description, Some(room_id.clone()), Some(Intent::Create));
            // Generation::start can still fail after the blockers passed (an
            // agent that dies instantly); it reports that by leaving no
            // generation running. Unblock the tool call rather than strand it.
            if !with_a2app(|state| state.generation.is_some()).unwrap_or(false) {
                let taken = with_a2app(|state| {
                    state.ai_generation_room = None;
                    state
                        .ai_sessions
                        .get_mut(room_id)
                        .and_then(|session| session.take_generation_answer())
                }).flatten();
                if let Some(answer) = taken.or_else(|| answer.take()) {
                    let _ = answer.send(Err(String::from(
                        "The build could not start; check the Mini Apps screen for the reason.",
                    )));
                }
            }
        }
        Effective::Denied | Effective::Undeclared => {
            let text = ai_tool_refused_text(cap.group.expect("apps.generate has a group"));
            note_ai_tool_call(room_id, "launch_splash_app", false, &text);
            let _ = answer.send(Err(text));
        }
        Effective::NeedsPrompt => {
            // First use: park the call and ask, exactly as a read's first use
            // does. The tool call waits on its serve thread until answered.
            let group = cap.group.expect("apps.generate has a group");
            queue_permission_prompt(
                cx,
                ui,
                subject,
                group,
                ParkedRequest::AiTool {
                    room_id: room_id.clone(),
                    job: SessionJob::LaunchSplashApp { description, answer },
                },
                None,
            );
        }
    }
}

/// The exact action a parked agent job performs, with the payload the worker
/// commits with, or `None` for a job with no exact-action layer.
#[cfg(unix)]
fn session_job_exact_action(
    room_id: &OwnedRoomId,
    job: &SessionJob,
) -> Option<(a2app_core::information_flow::SensitiveAction, serde_json::Value)> {
    use a2app_core::information_flow::SensitiveAction;
    match job {
        SessionJob::PostRoomMessage { room_id: target, text, .. } => Some((
            SensitiveAction { kind: "matrix.rooms.message.send".into(), target: target.clone() },
            serde_json::json!({ "room_id": target, "text": text }),
        )),
        SessionJob::LaunchSplashApp { description, .. } => Some((
            SensitiveAction { kind: "apps.generate".into(), target: room_id.to_string() },
            serde_json::json!({ "description": description, "room_id": room_id.to_string() }),
        )),
        _ => None,
    }
}

/// The newest pending host capture for `(context, epoch, action)`.
#[cfg(unix)]
fn pending_exact_decision(
    context: &a2app_core::information_flow::ContextId,
    epoch: u64,
    action: &a2app_core::information_flow::SensitiveAction,
) -> Option<(u64, a2app_core::information_flow::Influences)> {
    a2app_core::information_flow::recent_action_decisions().ok()?.into_iter().rev().find_map(|decision| {
        if &decision.context == context && decision.epoch == epoch && &decision.action == action {
            decision.request.map(|request| (request.id, decision.influences))
        } else {
            None
        }
    })
}

/// What the exact-action review modal shows: the exact target and the
/// host-captured payload, with no agent-authored prose.
#[cfg(unix)]
fn exact_review_info(
    action: &a2app_core::information_flow::SensitiveAction,
    payload: &serde_json::Value,
) -> TaskPromptInfo {
    let title = match action.kind.as_str() {
        "matrix.rooms.message.send" => format!("Post a message into room {}", action.target),
        "apps.generate" => format!("Build and run a mini-app in room {}", action.target),
        "mcp.tools.call" => format!("Call the mini-app tool \u{201c}{}\u{201d}", action.target),
        other => format!("Run {other} against {}", action.target),
    };
    TaskPromptInfo {
        explanation: String::from(
            "The assistant wants to do this, and it may have been influenced by content it read. \
             Review exactly what it will do before allowing it.",
        ),
        items: vec![TaskItemView {
            id: "exact".to_string(),
            title,
            detail: format!("Target: {}\n{}", action.target, serde_json::to_string_pretty(payload).unwrap_or_default()),
            chip: String::from("Needs review"),
            grantable: true,
            checked: true,
        }],
        risk: Some(String::from(
            "Approving allows exactly this one action. If the assistant reads more first, it asks again.",
        )),
    }
}

/// Checks an agent job's exact action. Returns the job to run when it may
/// proceed, or `None` after parking it behind the exact-action modal.
#[cfg(unix)]
fn park_exact_review_if_needed(
    cx: &mut Cx,
    ui: &WidgetRef,
    room_id: &OwnedRoomId,
    job: SessionJob,
) -> Option<SessionJob> {
    let Some((action, payload)) = session_job_exact_action(room_id, &job) else { return Some(job) };
    let Ok(context) = super::information_flow::agent_context(room_id.as_str()) else { return Some(job) };
    let Ok(epoch) = a2app_core::information_flow::context_epoch(&context) else { return Some(job) };
    if a2app_core::information_flow::check_exact_action_for_activation(&context, epoch, &action, &payload).is_ok() {
        return Some(job);
    }
    // The check failed for a reason other than review (for example a restarted
    // context) when there is no captured request to review.
    let Some((request_id, expected)) = pending_exact_decision(&context, epoch, &action) else { return Some(job) };
    let info = exact_review_info(&action, &payload);
    with_a2app(|state| state.exact_reviews.push_back(ExactReviewPrompt {
        room_id: room_id.clone(), context, epoch, action, payload, expected, request_id, info,
        resume: ExactResume::Job(job),
    }));
    show_next_permission_prompt(cx, ui);
    None
}

/// Refuses a parked effect (turn cancelled, session gone, room closed), so a
/// serve thread never hangs on an unanswered review.
#[cfg(unix)]
fn refuse_exact_resume(resume: ExactResume, message: String) {
    match resume {
        ExactResume::Job(job) => answer_session_job(job, Err(message)),
        ExactResume::AppTool { answer, .. } => { let _ = answer.send(Err(message)); }
    }
}

/// Resumes the effect after its review is approved.
#[cfg(unix)]
fn resume_exact(cx: &mut Cx, ui: &WidgetRef, room_id: OwnedRoomId, resume: ExactResume) {
    match resume {
        ExactResume::Job(job) => execute_session_job(cx, ui, &room_id, job),
        ExactResume::AppTool { tool, arguments, display_name, answer } =>
            run_app_tool_invocation(cx, ui, &room_id, tool, arguments, display_name, answer),
    }
}

/// Runs one tool call the room's agent made, on the UI thread, and sends the
/// result back to the serve thread that called the tool.
#[cfg(unix)]
fn execute_session_job(cx: &mut Cx, ui: &WidgetRef, room_id: &OwnedRoomId, job: SessionJob) {
    // Deriving tool-card details reads host metadata too. Label that input
    // before any description or result can be written back to the room.
    let recorded = super::information_flow::agent_context(room_id.as_str()).and_then(|context| {
        super::information_flow::current_context(&context)?;
        match &job {
            SessionJob::ReadTool { kind, .. } if is_directory_kind(kind) => {
                super::information_flow::record_directory_response(&context)?;
            }
            SessionJob::ReadTool { kind: ReadToolKind::OtherRoom { room, .. }, .. } => {
                a2app_core::information_flow::add_sources(&context, [super::information_flow::room_source(&context, room)])?;
            }
            SessionJob::LaunchApp { app_id, .. } => {
                let manifest = with_a2app(|state| state.registry.get(app_id).cloned()).flatten();
                a2app_core::information_flow::add_sources(&context, [super::information_flow::account_source(&context)])?;
                if let Some(manifest) = manifest {
                    join_manifest_code(&manifest, &context)?;
                }
            }
            SessionJob::CallMiniAppTool { tool, .. } | SessionJob::InvokeMiniAppTool { tool, .. } => {
                with_a2app(|state| {
                    if let Some(reg) = state.app_tools.get(tool) {
                        transfer_app_tool_provenance(state, &reg.app_id, reg.heap_key, room_id)?;
                    }
                    Ok::<(), String>(())
                }).unwrap_or_else(|| Err("Tool registry is unavailable.".into()))?;
            }
            _ => {}
        }
        Ok(())
    });
    if let Err(error) = recorded { answer_session_job(job, Err(super::information_flow::with_task_reask_hint(error))); return; }

    // After provenance is recorded, an effect that needs exact-action review
    // is parked behind the review modal instead of being dispatched.
    let Some(job) = park_exact_review_if_needed(cx, ui, room_id, job) else { return };

    // The job is the only place the call's arguments exist, so this is where
    // the human-readable target detail is computed and pinned to the call's
    // live row (reposted with it) and, later, its receipt chip.
    let rooms = cx.has_global::<RoomsListRef>().then(|| cx.get_global::<RoomsListRef>().clone());
    let detail = session_job_detail(rooms.as_ref(), &job);
    record_tool_call_detail(room_id, &ai_job_tool_name(&job), detail);
    match job {
        SessionJob::SendRoomMessage { text, answer } => {
            // The call's state row finishes when its message is on its way;
            // the receipt chip rides the same `ai_reply` card below. A failed
            // write rewrites both (see `AiRoomAction::PostReplyResult`).
            note_ai_tool_call(room_id, "send_message", true, "");
            // The tool is answered only once the write lands: the room can
            // refuse an `ai_reply` (the account needs state power), and the
            // model must hear that instead of "posted" — it is still inside
            // this call, so it can tell the user or try another way.
            let id = NEXT_AI_TOOL_ID.fetch_add(1, Ordering::Relaxed);
            with_a2app(|state| {
                let turn = state
                    .ai_rooms
                    .get(room_id)
                    .and_then(|info| info.active_turn.as_ref().map(|t| t.key.clone()));
                state.ai_replies.insert(id, (room_id.clone(), turn, answer, Vec::new()));
            });
            post_ai_reply(room_id, text, Some(id));
        }
        SessionJob::PostRoomMessage { room_id: target, text, answer } => {
            run_ai_room_post(cx, ui, room_id, target, text, answer);
        }
        SessionJob::LaunchSplashApp { description, answer } => {
            run_ai_generation(cx, ui, room_id, description, answer);
        }
        SessionJob::ListApps { answer } => {
            run_ai_list_apps(cx, ui, room_id, answer);
        }
        SessionJob::LaunchApp { app_id, answer } => {
            run_ai_launch_app(cx, ui, room_id, app_id, answer);
        }
        SessionJob::ReadTool { kind, answer } => {
            run_ai_read_tool(cx, ui, room_id, kind, answer);
        }
        SessionJob::RequestTaskPermissions { request, answer } => {
            run_request_task_permissions(cx, ui, room_id, request, answer);
        }
        SessionJob::FetchUrl { url, answer } => run_ai_fetch(cx, ui, room_id, url, answer),
        SessionJob::NetworkAccess { tool, host, url, answer } => {
            run_network_access(cx, ui, room_id, &tool, &host, &url, answer);
        }
        SessionJob::InvokeMiniAppTool { tool, arguments, answer } => {
            run_mini_app_tool_call(cx, ui, room_id, tool, arguments, false, answer);
        }
        SessionJob::CallMiniAppTool { tool, arguments, answer } => {
            run_mini_app_tool_call(cx, ui, room_id, tool, arguments, true, answer);
        }
        SessionJob::ListMiniAppTools { answer } => {
            run_ai_list_mini_app_tools(room_id, answer);
        }
    }
}

/// Resolves exactly the room-local tool name that both task grants and an
/// invocation check. A raw name is accepted only when it is unambiguous.
#[cfg(unix)]
fn canonical_app_tool_name(state: &A2AppState, room_id: &OwnedRoomId, tool: &str) -> Option<String> {
    if let Some(registration) = state.app_tools.get(tool) {
        return (&registration.room_id == room_id).then(|| registration.full_name.clone());
    }
    let mut matches = state.app_tools.values()
        .filter(|registration| &registration.room_id == room_id && registration.raw_name == tool);
    let first = matches.next()?;
    matches.next().is_none().then(|| first.full_name.clone())
}

/// Gate 2 for a mini-app tool: an invocation — whether the model called the
/// tool directly or through the stable `call_mini_app_tool` bridge — is
/// granted per tool for this room's AI; first use parks behind a prompt that
/// shows the tool and the arguments. The `McpTools` group is a kill switch
/// only.
///
/// `via_bridge` changes only which job name the turn card is closed under: the
/// agent reports the bridge call as `call_mini_app_tool`, while a direct call
/// carries the tool's own namespaced name.
#[cfg(unix)]
fn run_mini_app_tool_call(
    cx: &mut Cx,
    ui: &WidgetRef,
    room_id: &OwnedRoomId,
    tool: String,
    arguments: serde_json::Map<String, serde_json::Value>,
    via_bridge: bool,
    answer: Sender<Result<String, String>>,
) {
    // The bridge takes a free-form id, so a name the model invented must be
    // refused rather than prompt for a tool nobody registered. The same check
    // covers a direct call for a tool withdrawn since the agent listed it.
    //
    // Accept either the namespaced `tool` id from `list_mini_app_tools` or
    // the app's own name: the app's room message says `ttt_play`, while the
    // list exposes `app_tic-tac-toe_ttt_play`, and the model should not have
    // to know which is which. A raw name matching more than one tool in the
    // room is ambiguous and refused rather than guessed at.
    let resolved_tool = with_a2app(|state| canonical_app_tool_name(state, room_id, &tool)).flatten();
    let Some(tool) = resolved_tool else {
        let reason = format!(
            "There is no mini-app tool `{tool}` registered in this room. \
             Use list_mini_app_tools to see the tools the apps offer."
        );
        let _ = answer.send(Err(reason));
        return;
    };
    let display_name = if via_bridge { "call_mini_app_tool".to_string() } else { tool.clone() };
    let subject = agent_subject(room_id.as_str());
    let decided = with_a2app(|state| {
        if state.permissions.state(&subject, Permission::McpTools) == GrantState::Denied {
            return Effective::Denied;
        }
        if state.permissions.has_scoped_tool_grant(&subject, &tool, "", PermissionContext {
            origin_room: Some(room_id.as_str()), target_room: Some(room_id.as_str()),
        }) { Effective::Granted } else { state.permissions.tool_effective(&subject, &tool, None) }
    })
    .unwrap_or(Effective::NeedsPrompt);
    match decided {
        Effective::Granted => {
            run_app_tool_invocation(cx, ui, room_id, tool, arguments, display_name, answer);
        }
        Effective::NeedsPrompt => {
            let grant = with_a2app(|state| {
                state.app_tools.get(&tool).map(|reg| PromptToolGrant {
                    full_name: tool.clone(),
                    name: tool.clone(),
                    description: reg.description.clone(),
                    args: reg.args.clone(),
                    content_hash: String::new(),
                })
            })
            .flatten();
            // The parked job must remember the bridge so the replay opens the
            // same turn-card name the agent already reported.
            let job = if via_bridge {
                SessionJob::CallMiniAppTool { tool, arguments, answer }
            } else {
                SessionJob::InvokeMiniAppTool { tool, arguments, answer }
            };
            queue_permission_prompt(
                cx,
                ui,
                subject,
                Permission::McpTools,
                ParkedRequest::AiTool { room_id: room_id.clone(), job },
                grant,
            );
        }
        Effective::Denied | Effective::Undeclared => {
            let reason = format!(
                "The user did not allow the AI to use the mini-app tool \"{}\".",
                tool
            );
            note_ai_tool_call(room_id, &display_name, false, &reason);
            let _ = answer.send(Err(reason));
        }
    }
}

/// Executes the `list_mini_app_tools` tool: the tools mini-apps registered on
/// this room's session, as JSON the model reads to pick a `tool` id for
/// `call_mini_app_tool`. Metadata only — the app's source and data never leave
/// it. A group `Denied` is the kill switch for the whole feature, so it
/// reports none.
#[cfg(unix)]
fn run_ai_list_mini_app_tools(
    room_id: &OwnedRoomId,
    answer: Sender<Result<String, String>>,
) {
    let subject = agent_subject(room_id.as_str());
    let denied = with_a2app(|state| {
        state.permissions.state(&subject, Permission::McpTools) == GrantState::Denied
    })
    .unwrap_or(false);
    if denied {
        note_ai_tool_call(room_id, "list_mini_app_tools", false, "");
        let _ = answer.send(Err(String::from(
            "The user has turned off mini-app tools for this room's AI.",
        )));
        return;
    }
    let tools = with_a2app(|state| {
        state
            .app_tools
            .values()
            .filter(|reg| &reg.room_id == room_id)
            .map(|reg| {
                serde_json::json!({
                    "tool": reg.full_name,
                    "name": reg.raw_name,
                    "description": reg.description,
                    "args": reg.args.iter().map(|(name, ty, desc)| serde_json::json!({
                        "name": name,
                        "type": ty,
                        "description": desc,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();
    let transfer = with_a2app(|state| {
        for reg in state.app_tools.values().filter(|reg| &reg.room_id == room_id) {
            transfer_app_tool_provenance(state, &reg.app_id, reg.heap_key, room_id)?;
        }
        Ok::<(), String>(())
    }).unwrap_or_else(|| Err("Tool registry is unavailable.".into()));
    if let Err(error) = transfer { let _ = answer.send(Err(super::information_flow::with_task_reask_hint(error))); return; }
    let text = serde_json::json!({ "tools": tools }).to_string();
    note_ai_tool_call(room_id, "list_mini_app_tools", true, "");
    let _ = answer.send(Ok(text));
}

/// Delivers one app-tool invocation into the owning isolate as
/// `on_tool_call`, parking the model's serve thread in `app_tool_calls` until
/// the app answers `mcp.tools.result` (or [`APP_TOOL_TIMEOUT`] sweeps it).
#[cfg(unix)]
fn run_app_tool_invocation(
    cx: &mut Cx,
    ui: &WidgetRef,
    room_id: &OwnedRoomId,
    tool: String,
    arguments: serde_json::Map<String, serde_json::Value>,
    display_name: String,
    answer: Sender<Result<String, String>>,
) {
    let Some((app_id, heap_key, raw_name)) = with_a2app(|state| {
        state
            .app_tools
            .get(&tool)
            .map(|reg| (reg.app_id.clone(), reg.heap_key, reg.raw_name.clone()))
    })
    .flatten()
    else {
        let _ = answer.send(Err(format!("the mini-app tool `{tool}` is no longer registered")));
        return;
    };
    // The owning app's kill switch gates each call, not just registration: a
    // Deny made between calls stops this one and takes its tools back.
    if !app_tools_allowed(&app_id) {
        withdraw_app_tools(&app_id);
        let _ = answer.send(Err(format!("the mini-app tool `{tool}` is no longer allowed")));
        return;
    }
    let transfer = super::information_flow::prepare_agent(room_id.as_str()).and_then(|from| {
        let to = super::information_flow::context_for_heap(heap_key)?;
        if !matches!(&to, a2app_core::information_flow::ContextId::App { app, room: Some(room), .. }
            if app == &app_id && room == room_id.as_str())
        { return Err("The mini-app tool's owning instance changed.".into()); }
        a2app_core::information_flow::transfer(&from, &to)?;
        // Tool metadata and the fact of invocation are observable to the agent too.
        a2app_core::information_flow::transfer(&to, &from)?;
        let app_activation = a2app_core::information_flow::context_epoch(&to)?;
        Ok((from, app_activation))
    });
    let (context, app_activation) = match transfer { Ok(value) => value, Err(error) => { let _ = answer.send(Err(super::information_flow::with_task_reask_hint(error))); return; } };
    // An app-tool call is a privileged effect: after untrusted influence it is
    // parked behind the exact-action review. The transfers above are
    // idempotent, so the resumed invocation commits against the same set.
    let action = a2app_core::information_flow::SensitiveAction { kind: "mcp.tools.call".into(), target: tool.clone() };
    let exact_payload = serde_json::json!({
        "tool": tool.clone(), "arguments": arguments.clone(), "app_id": app_id.clone(),
        "app_activation": app_activation,
    });
    let epoch = a2app_core::information_flow::context_epoch(&context).unwrap_or(0);
    match a2app_core::information_flow::commit_exact_action_for_activation(&context, epoch, &action, &exact_payload) {
        Ok(()) => {}
        Err(error) => {
            if let Some((request_id, expected)) = pending_exact_decision(&context, epoch, &action) {
                let info = exact_review_info(&action, &exact_payload);
                with_a2app(|state| state.exact_reviews.push_back(ExactReviewPrompt {
                    room_id: room_id.clone(), context, epoch, action, payload: exact_payload, expected, request_id, info,
                    resume: ExactResume::AppTool { tool, arguments, display_name, answer },
                }));
                show_next_permission_prompt(cx, ui);
                return;
            }
            let _ = answer.send(Err(super::information_flow::with_task_reask_hint(error)));
            return;
        }
    }
    let call_id = NEXT_APP_TOOL_CALL_ID.fetch_add(1, Ordering::Relaxed);
    let audit = a2app_core::protection_audit::Attempt::start(&context, None, a2app_core::protection_audit::ActivityKind::ToolCall);
    with_a2app(|state| {
        state.app_tool_calls.insert(
            call_id,
            AppToolPending {
                room_id: room_id.clone(),
                app_id,
                heap_key,
                full_name: tool.clone(),
                display_name,
                answer,
                since: Instant::now(),
                audit,
            },
        );
    });
    let payload = serde_json::json!({
        "call_id": call_id,
        "tool": tool,
        // The app's own name, so it routes without knowing the `app_<id>_`
        // prefix the runtime chose.
        "name": raw_name,
        "arguments": arguments,
    })
    .to_string();
    let called = instances::call_hook_by_heap(cx, heap_key, live_id!(on_tool_call), &[&payload]);
    if !called {
        // The app is gone or never defined the hook; answer at once rather
        // than leave the model waiting for the timeout.
        if let Some(pending) = with_a2app(|state| state.app_tool_calls.remove(&call_id)).flatten() {
            pending.audit.finish(false);
            let _ = pending.answer.send(Err(format!(
                "the mini-app did not handle the tool call `{tool}`"
            )));
        }
    }
}

#[cfg(unix)]
fn cancel_ai_fetches(room_id: &OwnedRoomId) {
    with_a2app(|state| {
        state.ai_fetches.retain(|_, (room, _, alive)| {
            if room != room_id { return true; }
            alive.store(false, std::sync::atomic::Ordering::SeqCst);
            false
        });
    });
}

/// The model's internet tool uses the same transport as mini-apps.
#[cfg(unix)]
fn run_ai_fetch(cx: &mut Cx, ui: &WidgetRef, room_id: &OwnedRoomId, url: String, answer: Sender<Result<String, String>>) {
    let prepared = super::information_flow::agent_context(room_id.as_str()).and_then(|context| {
        super::information_flow::current_context(&context)?;
        let recipient = a2app_core::information_flow::Recipient::network_origin(&url)?;
        a2app_core::information_flow::ensure_allowed(&context, &recipient)?;
        let request = super::network::Request::parse(&serde_json::json!({ "url": url }))?;
        Ok((context, request))
    });
    let (context, request) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => { let _ = answer.send(Err(super::information_flow::with_task_reask_hint(error))); return; }
    };
    let subject = agent_subject(room_id.as_str());
    let consent = with_a2app(|state| {
        let permissions = &state.permissions;
        let denied = permissions.is_restricted(&subject)
            || permissions.state(&subject, Permission::Network) == GrantState::Denied
            || permissions.capability_state(&subject, "network.http") == GrantState::Denied
            || url::Url::parse(&url).ok().and_then(|url| url.host_str().map(str::to_owned))
                .is_some_and(|host| state.dismissed_net_hosts.contains(&(subject.clone(), host)));
        if denied { return Err("Internet permission is blocked for this agent."); }
        Ok(permissions.is_url_allowed(&subject, &url, PermissionContext {
            origin_room: Some(room_id.as_str()), target_room: Some(room_id.as_str()),
        }).then(|| permissions.clone()))
    }).unwrap_or(Err("Permissions are unavailable."));
    let consent = match consent {
        Ok(Some(consent)) => consent,
        Ok(None) => {
            queue_permission_prompt(cx, ui, subject, Permission::Network,
                ParkedRequest::AiTool { room_id: room_id.clone(), job: SessionJob::FetchUrl { url, answer } }, None);
            return;
        }
        Err(error) => { let _ = answer.send(Err(error.into())); return; }
    };
    let id = NEXT_AI_TOOL_ID.fetch_add(1, Ordering::Relaxed);
    let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    with_a2app(|state| { state.ai_fetches.insert(id, (room_id.clone(), answer, alive.clone())); });
    let origin = Some(room_id.to_string());
    crate::sliding_sync::spawn_async_task(async move {
        let result = super::network::run(request, context.clone(), subject, origin, consent, Some(alive)).await;
        Cx::post_action(AgentNetworkResult { id, context, result });
    });
}

/// Executes one internet-access request from the agent's own web tool: the
/// same permission store the MCP tools use decides, but PER HOST. An already
/// allowed host (durable or this session) passes immediately; a group `Denied`
/// or a session "Not Now" refuses without asking; otherwise the request parks
/// behind the permission prompt and the web tool blocks until the user
/// answers. On Allow the host joins the room subject's allowlist — exactly
/// that host, never the whole internet.
#[cfg(unix)]
fn run_network_access(
    cx: &mut Cx,
    ui: &WidgetRef,
    room_id: &OwnedRoomId,
    tool: &str,
    host: &str,
    url: &str,
    answer: Sender<Result<String, String>>,
) {
    let allowed = super::information_flow::prepare_agent(room_id.as_str()).and_then(|context| {
        super::information_flow::current_context(&context)?;
        let recipient = a2app_core::information_flow::Recipient::network_origin(url)?;
        a2app_core::information_flow::ensure_allowed(&context, &recipient)
    });
    if let Err(error) = allowed { let _ = answer.send(Err(super::information_flow::with_task_reask_hint(error))); return; }
    let subject = agent_subject(room_id.as_str());
    let verdict = with_a2app(|state| {
        if state.permissions.is_restricted(&subject) {
            return NetworkVerdict::Denied;
        }
        // A durable group Denied is the kill switch for all internet access.
        if state.permissions.state(&subject, Permission::Network) == GrantState::Denied {
            return NetworkVerdict::Denied;
        }
        if state.permissions.is_url_allowed(&subject, url, PermissionContext {
            origin_room: Some(room_id.as_str()), target_room: Some(room_id.as_str()),
        }) {
            return NetworkVerdict::Granted;
        }
        // "Not Now" for THIS host this session: refuse without re-asking, so
        // a looping web tool cannot nag its way to an accidental Allow. Scoped
        // to the host — dismissing one site never silently refuses another.
        if state
            .dismissed_net_hosts
            .contains(&(subject.clone(), host.to_ascii_lowercase()))
        {
            return NetworkVerdict::Denied;
        }
        // A group-level "Not Now" (only ever set for non-network permissions)
        // still refuses, as a fallback.
        if state.dismissed_prompts.contains(&(subject.clone(), Permission::Network)) {
            return NetworkVerdict::Denied;
        }
        NetworkVerdict::NeedsPrompt
    })
    .unwrap_or(NetworkVerdict::Denied);
    match verdict {
        NetworkVerdict::Granted => {
            let _ = answer.send(Ok(String::from("allowed")));
        }
        NetworkVerdict::Denied => {
            let _ = answer.send(Err(format!(
                "The user did not allow the AI in this room to reach `{host}`. \
                 Tell them what you wanted to fetch and why, so they can allow it."
            )));
        }
        NetworkVerdict::NeedsPrompt => {
            let _ = tool;
            queue_permission_prompt(
                cx,
                ui,
                subject,
                Permission::Network,
                ParkedRequest::NetworkAccess {
                    room_id: room_id.clone(),
                    host: host.to_string(),
                    url: url.to_string(),
                    answer,
                },
                None,
            );
        }
    }
}

/// The three-way decision for one internet host, mirroring [`Effective`] but
/// with the per-host allowlist layered in.
#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum NetworkVerdict {
    Granted,
    Denied,
    NeedsPrompt,
}

/// Resolves a finished (or cancelled) generation back to the session that
/// launched it through `launch_splash_app`: answers the waiting tool call
/// with the outcome, and when an app was actually built and the session is
/// still alive, docks it into the room's own pane (the tool's "then runs
/// it"). A generation the user started from the Mini Apps screen resolves to
/// nothing here — `ai_generation_room` is unset for those.
#[cfg(unix)]
fn resolve_session_generation(
    cx: &mut Cx,
    ui: &WidgetRef,
    manifest: Option<&MiniAppManifest>,
    outcome: Result<String, String>,
) {
    let room_id = with_a2app(|state| state.ai_generation_room.take()).flatten();
    let Some(room_id) = room_id else { return };
    // Take the parked answer OUT of the borrow first: sending it and
    // recording the receipt must run after `with_a2app` releases its guard,
    // because both touch the same state again (`note_ai_tool_call` borrows it
    // a second time) — nesting them inside this closure panicked with a
    // re-entrant RefCell borrow.
    let Some(answer) = with_a2app(|state| {
        state
            .ai_sessions
            .get_mut(&room_id)
            .and_then(|session| session.take_generation_answer())
    })
    .flatten()
    else {
        return;
    };
    let (summary, ok) = match &outcome {
        Ok(_) => (String::new(), true),
        Err(e) => (e.clone(), false),
    };
    note_ai_tool_call(&room_id, "launch_splash_app", ok, &summary);
    let _ = answer.send(outcome.clone());
    if outcome.is_ok() {
        if let Some(manifest) = manifest {
            if let Ok(context) = super::information_flow::agent_context(room_id.as_str()) {
                cx.action(A2AppOp::OpenAppFromContext {
                    flow_epoch: a2app_core::information_flow::context_epoch(&context).unwrap_or(0),
                    context, app_id: manifest.id.clone(), room_id,
                });
            }
        }
    }
    ui.redraw(cx);
}

/// Milliseconds since the UNIX epoch, for the `createdAt` fields of the
/// AI-session state rows.
#[cfg(unix)]
fn ai_now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Records one tool call outcome on the room's pending receipt list, shown on
/// the turn's `ai_reply` card. Refusals and failures are exactly what a
/// receipt exists for, so the summary is kept short; a granted read that
/// succeeded leaves an empty summary.
///
/// The outcome also rewrites the call's `ai_tool_call` state row from
/// `Started` to `Done` (matching the oldest still-running row with this tool
/// name), so the room's live tool log shows each call finishing.
#[cfg(unix)]
fn note_ai_tool_call(room_id: &OwnedRoomId, name: &str, ok: bool, summary: &str) {
    // The summary is rendered for the turn card by `finish_ai_tool_call` (and
    // independently truncated for the receipt chip), so a raw JSON result is
    // formatted once there rather than stored and re-parsed later.
    // `finish_ai_tool_call` returns the target detail recorded for the call
    // when its job reached the UI thread, so the receipt chip names the same
    // room/space/app the live row did.
    let detail = finish_ai_tool_call(room_id, name, ok, summary);
    push_ai_tool_receipt(room_id, name, detail, ok, summary);
}

/// Adds one finished call to the room's pending receipt list, shown on the
/// turn's `ai_reply` card. Split out of [`note_ai_tool_call`] so a caller
/// that already finished the live row (the async read result, which may skip
/// the receipt for ungated memory reads) does not finish it twice and lose
/// the call's target detail.
#[cfg(unix)]
fn push_ai_tool_receipt(
    room_id: &OwnedRoomId,
    name: &str,
    detail: Option<String>,
    ok: bool,
    summary: &str,
) {
    with_a2app(|state| {
        if let Some(info) = state.ai_rooms.get_mut(room_id) {
            info.pending_tool_calls.push(AiReplyToolCall {
                name: name.to_string(),
                detail,
                ok,
                summary: crate::a2app::ai_room_events::format_tool_result(name, summary).chars().take(160).collect(),
            });
        }
    });
}

/// Rewrites the turn's `ai_turn` state row from `Started` to `Done` for one
/// tool call — the live half of [`note_ai_tool_call`], separated so a call
/// that should leave no receipt chip (ungated `read_room_memory`) can still
/// finish its line inside the turn card. Matches the oldest still-running
/// call with this tool name; a call with no match is left alone. Returns the
/// call's target detail (for the turn's receipt chip).
#[cfg(unix)]
fn finish_ai_tool_call(room_id: &OwnedRoomId, name: &str, ok: bool, summary: &str) -> Option<String> {
    let mut detail_out = None;
    let posted = with_turn(room_id, AiTurnStatus::Running, false, None, |calls| {
        let pos = calls
            .iter()
            .position(|c| c.name == name && c.status == AiTurnToolStatus::Started)
            .or_else(|| calls.iter().position(|c| c.name == name));
        let Some(pos) = pos else { return false };
        let call = &mut calls[pos];
        call.status = AiTurnToolStatus::Done;
        call.ok = ok;
        call.summary = crate::a2app::ai_room_events::format_tool_result(name, summary).chars().take(600).collect();
        detail_out = call.detail.clone();
        true
    });
    if let Some((key, content, changed)) = posted {
        // A completion with no matching running call (e.g. the agent's own
        // tool firing twice) changes nothing; skip the redundant rewrite so
        // it does not race the real snapshots.
        if changed {
            post_ai_turn(room_id, &key, &content);
        }
    }
    detail_out
}

/// Applies one tool call's human-readable target detail to its line in the
/// turn card and reposts the turn. Called from [`execute_session_job`] as soon
/// as the job — which holds the arguments — reaches the UI thread. The ACP
/// `started` event only carries the tool name, so it opens the call without a
/// detail; this reposts the same turn with the detail once known. If the job
/// wins the race and arrives before the ACP event, it creates the call and
/// [`on_ai_tool_call_started`] then leaves it alone.
#[cfg(unix)]
fn record_tool_call_detail(room_id: &OwnedRoomId, name: &str, detail: Option<String>) {
    if detail.is_none() {
        return;
    }
    let posted = with_turn(room_id, AiTurnStatus::Running, true, Some(false), |calls| {
        // Match the OLDEST still-running call with this name (mirroring
        // `finish_ai_tool_call`), falling back to any call with the name so a
        // job that lost the race to a `Done` rewrite can still fill in its
        // detail. Matching by name alone picked the first call, so repeated
        // tool names attached the detail to the wrong line.
        let pos = calls
            .iter()
            .position(|c| c.name == name && c.status == AiTurnToolStatus::Started)
            .or_else(|| calls.iter().position(|c| c.name == name));
        match pos {
            Some(pos) => {
                if calls[pos].detail == detail {
                    return false;
                }
                calls[pos].detail = detail;
                true
            }
            None => {
                calls.push(AiTurnToolCall {
                    name: name.to_string(),
                    detail,
                    status: AiTurnToolStatus::Started,
                    ok: false,
                    summary: String::new(),
                });
                true
            }
        }
    });
    if let Some((key, content, changed)) = posted {
        if changed {
            post_ai_turn(room_id, &key, &content);
        }
    }
}

/// Posts one `ai_activity` state row with a fresh key — an append-only
/// marker in the room's transcript that the agent is doing something visible
/// (a turn error or the session stopping). `thinking` is not posted as an
/// activity row: it lives inside the open turn's card.
#[cfg(unix)]
fn post_ai_activity(room_id: &OwnedRoomId, kind: AiActivityKind, label: Option<&str>) {
    let content = AiActivityContent {
        v: 1,
        kind,
        label: label.map(str::to_string),
        created_at: ai_now_millis(),
    };
    // Error/stopped rows are a closed turn's final output: register the write
    // so the turn's grants stay live until it lands.
    match post_ai_state_event(room_id, AI_ACTIVITY_EVENT_TYPE, &next_ai_state_key("activity"), &content) {
        Some(token) => register_final_write(room_id, token),
        // The row could not even be queued; it is not outstanding.
        None => settle_closed_turn(room_id),
    }
}

/// Opens the turn's card (creating it on the turn's first tool call) and
/// records one started call, then reposts the turn. The call is skipped when
/// `execute_session_job` already opened it with its detail.
#[cfg(unix)]
fn on_ai_tool_call_started(room_id: &OwnedRoomId, name: &str) {
    let posted = with_turn(room_id, AiTurnStatus::Running, true, Some(false), |calls| {
        if calls
            .iter()
            .any(|c| c.name == name && c.status == AiTurnToolStatus::Started)
        {
            return false;
        }
        calls.push(AiTurnToolCall {
            name: name.to_string(),
            detail: None,
            status: AiTurnToolStatus::Started,
            ok: false,
            summary: String::new(),
        });
        true
    });
    if let Some((key, content, changed)) = posted {
        if changed {
            post_ai_turn(room_id, &key, &content);
        }
    }
}

/// Opens the turn's card on the model's first `Thought` (creating it if the
/// turn had not started yet) and marks it `thinking`, so a turn that reasons
/// before it calls a tool still shows a warm, spinning card with a
/// `💭 Thinking…` line. Reposts only when the marker actually changed.
#[cfg(unix)]
fn note_ai_turn_thinking(room_id: &OwnedRoomId) {
    let posted = with_turn(room_id, AiTurnStatus::Running, true, Some(true), |_| false);
    if let Some((key, content, changed)) = posted {
        if changed {
            post_ai_turn(room_id, &key, &content);
        }
    }
}

/// Runs `f` against this room's open turn, then returns the turn's state key
/// and a fresh [`AiTurnContent`] snapshot to repost. When `create` is true a
/// missing turn is minted first (the turn's first activity); when false an
/// absent turn yields `None` (an outcome for a call that never started must
/// not conjure an empty card). `thinking` sets the turn's reasoning marker
/// when given. The closure returns whether it actually changed the tool list,
/// which the caller uses to skip a redundant rewrite. The snapshot carries a
/// bumped `seq` either way, so the renderer can always pick the newest.
#[cfg(unix)]
fn with_turn(
    room_id: &OwnedRoomId,
    status: AiTurnStatus,
    create: bool,
    thinking: Option<bool>,
    f: impl FnOnce(&mut Vec<AiTurnToolCall>) -> bool,
) -> Option<(String, AiTurnContent, bool)> {
    with_a2app(|state| {
        let info = state.ai_rooms.get_mut(room_id)?;
        let created = create && info.active_turn.is_none();
        if created {
            info.active_turn = Some(ActiveTurn {
                key: next_ai_state_key("turn"),
                tool_calls: Vec::new(),
                thinking: false,
                seq: 0,
                created_at: ai_now_millis(),
            });
            // A fresh turn starts from the base spacing; the adaptive backoff
            // only widens it again if this turn's writes are rejected.
            info.ai_turn_backoff = AI_TURN_POST_MIN_INTERVAL;
        }
        let turn = info.active_turn.as_mut()?;
        let mut changed = created;
        if let Some(thinking) = thinking {
            if turn.thinking != thinking {
                turn.thinking = thinking;
                changed = true;
            }
        }
        changed |= f(&mut turn.tool_calls);
        turn.seq = turn.seq.saturating_add(1);
        Some((
            turn.key.clone(),
            AiTurnContent {
                v: 1,
                turn: turn.key.clone(),
                first: Some(created),
                status,
                seq: turn.seq,
                thinking: turn.thinking,
                tool_calls: turn.tool_calls.clone(),
                created_at: turn.created_at,
            },
            changed,
        ))
    })
    .flatten()
}

/// Registers one final write a closed turn must land before its task grants
/// can be revoked.
#[cfg(unix)]
fn register_final_write(room_id: &OwnedRoomId, token: u64) {
    with_a2app(|state| {
        if let Some(info) = state.ai_rooms.get_mut(room_id) {
            info.pending_final_writes.insert(token);
        }
    });
}

/// Releases one final-write token and revokes the closed turn's grants once
/// every token is gone.
#[cfg(unix)]
fn final_write_landed(room_id: &OwnedRoomId, token: u64) {
    with_a2app(|state| {
        if let Some(info) = state.ai_rooms.get_mut(room_id) {
            info.pending_final_writes.remove(&token);
        }
    });
    settle_closed_turn(room_id);
}

/// Revokes a closed turn's grants once its `Done` snapshot is no longer queued,
/// every outstanding final-write token has landed. The host holds the next
/// member turn until this rollback completes, so a late activity notification
/// cannot retain the closed turn's grants by opening a stray turn card.
///
/// A dead session's teardown waits here too: its flow context must outlive the
/// writes it still has queued (`Gone`).
#[cfg(unix)]
fn settle_closed_turn(room_id: &OwnedRoomId) {
    let (revoke, retire) = with_a2app(|state| {
        let info = state.ai_rooms.get_mut(room_id)?;
        if !info.final_writes_pending_revoke
            || !info.pending_final_turns.is_empty()
            || !info.pending_final_writes.is_empty()
        {
            return Some((false, false));
        }
        info.final_writes_pending_revoke = false;
        info.final_writes_deadline = None;
        Some((true, info.retire_after_final_writes))
    })
    .flatten()
    .unwrap_or((false, false));
    if revoke {
        revoke_task_grants(room_id);
    }
    if retire {
        with_a2app(|state| {
            if let Some(info) = state.ai_rooms.get_mut(room_id) {
                info.retire_after_final_writes = false;
            }
        });
        stop_ai_session(room_id);
    }
}

/// Settles and clears this room's open turn: rewrites its `ai_turn` row
/// `Done` (turning the card's tint neutral and hiding its spinner) and drops
/// it, so the next turn gets a fresh card. A no-op when no turn is open.
///
/// When `revoke_grants` is false, the turn's task grants are kept until its
/// final writes land: this queues the `Done` snapshot (tracked by
/// [`AiRoomInfo::pending_final_turns`] until the flush submits it), and the caller
/// finishes with [`settle_closed_turn`] after it has queued the turn's reply or
/// error/stopped row. Those later writes register their own tokens, so the last
/// one to land revokes. `revoke_grants` true revokes at once, for the rare
/// close with no write to wait for (session teardown uses `revoke_task_grants`
/// directly).
#[cfg(unix)]
fn close_active_turn(room_id: &OwnedRoomId, revoke_grants: bool) {
    let closed = with_a2app(|state| {
        let info = state.ai_rooms.get_mut(room_id)?;
        let mut turn = info.active_turn.take()?;
        // The final snapshot gets the highest `seq` of the turn, so it wins the
        // renderer's newest-snapshot selection even if it lands before an
        // earlier rewrite.
        turn.seq = turn.seq.saturating_add(1);
        if !revoke_grants {
            info.final_writes_pending_revoke = true;
            info.pending_final_turns.insert(turn.key.clone());
            info.final_writes_deadline = Some(Instant::now() + FINAL_WRITE_FAILSAFE);
        }
        Some((
            turn.key.clone(),
            AiTurnContent {
                v: 1,
                turn: turn.key.clone(),
                first: Some(false),
                status: AiTurnStatus::Done,
                seq: turn.seq,
                thinking: turn.thinking,
                tool_calls: turn.tool_calls,
                created_at: turn.created_at,
            },
        ))
    })
    .flatten();
    match closed {
        Some((key, content)) => {
            post_ai_turn(room_id, &key, &content);
            if revoke_grants {
                revoke_task_grants(room_id);
            }
        }
        None if revoke_grants => revoke_task_grants(room_id),
        None => {
            // No turn card was open, so there is no Done snapshot. The caller
            // still has to queue the error/stopped row or the reply, then
            // settle; do not revoke here, or that write would be refused.
            with_a2app(|state| {
                if let Some(info) = state.ai_rooms.get_mut(room_id) {
                    info.final_writes_pending_revoke = true;
                    info.final_writes_deadline = Some(Instant::now() + FINAL_WRITE_FAILSAFE);
                }
            });
        }
    }
}

/// The starting minimum spacing between `ai_turn` state-event writes for one
/// room. matrix.org responds `429 M_LIMIT_EXCEEDED` to state events written
/// faster than roughly this (`retry_after` was observed at 4s), and a busy
/// turn can otherwise produce dozens of snapshots.
#[cfg(unix)]
const AI_TURN_POST_MIN_INTERVAL: Duration = Duration::from_secs(5);

/// Whether a room's next `ai_turn` snapshot may be written now. A turn's first
/// snapshot is the card's anchor and skips the ADAPTIVE backoff so the card
/// appears at once, but it still waits out the hard minimum spacing: a run of
/// very short turns must not fire back-to-back state events into the server's
/// rate limit. Every later snapshot respects the adaptive backoff.
#[cfg(unix)]
fn turn_post_is_cooled_down(
    anchor_pending: bool,
    since_last: Option<Duration>,
    backoff: Duration,
) -> bool {
    let anchor_cooled = anchor_pending
        && since_last.is_none_or(|elapsed| elapsed >= AI_TURN_POST_MIN_INTERVAL);
    let spaced = since_last.is_none_or(|elapsed| elapsed >= backoff);
    anchor_cooled || spaced
}
/// The upper bound the adaptive `ai_turn` write spacing backs off to when the
/// homeserver keeps rejecting writes as rate-limited.
#[cfg(unix)]
const AI_TURN_POST_MAX_INTERVAL: Duration = Duration::from_secs(60);

/// Queues one `ai_turn` snapshot for this room, replacing an earlier pending
/// one of the SAME turn: only a turn's newest snapshot matters, so coalescing
/// a burst of updates into a single write is what keeps Robrix under the
/// homeserver's state-event rate limit. Another turn's pending snapshot (the
/// previous turn's final `Done`) stays queued ahead of it. The actual write
/// happens in [`flush_pending_ai_turns`].
#[cfg(unix)]
fn post_ai_turn(room_id: &OwnedRoomId, key: &str, content: &AiTurnContent) {
    with_a2app(|state| {
        let Some(info) = state.ai_rooms.get_mut(room_id) else { return };
        if let Some(slot) = info.pending_ai_turns.iter_mut().find(|(k, _)| k == key) {
            slot.1 = content.clone();
        } else {
            info.pending_ai_turns.push_back((key.to_string(), content.clone()));
        }
    });
}

/// Writes at most one coalesced `ai_turn` snapshot per room per its adaptive
/// spacing (starting at [`AI_TURN_POST_MIN_INTERVAL`]), and only once the
/// previous write has finished. Overlapping writes are what turned a single
/// 429 into a retry storm: the SDK retries a rate-limited state event
/// (honoring `retry_after`), so the room must stop producing new ones until
/// that settles. Called every runtime pass; also keeps a timer alive so the
/// final snapshot of an otherwise idle turn still goes out.
#[cfg(unix)]
fn flush_pending_ai_turns(cx: &mut Cx) {
    let now = Instant::now();
    // Failsafe: a closed turn whose final write never reports back must not
    // keep its grants alive forever. The flush timer calls this every pass.
    let expired: Vec<OwnedRoomId> = with_a2app(|state| {
        state
            .ai_rooms
            .iter_mut()
            .filter(|(_, info)| {
                info.final_writes_pending_revoke
                    && info.final_writes_deadline.is_some_and(|deadline| now >= deadline)
            })
            .map(|(room_id, info)| {
                info.pending_final_writes.clear();
                info.pending_final_turns.clear();
                room_id.clone()
            })
            .collect()
    })
    .unwrap_or_default();
    for room_id in expired {
        log!("AI Rooms: room {room_id}'s final writes did not report back in time; releasing its turn grants.");
        settle_closed_turn(&room_id);
    }
    let mut to_post: Vec<(OwnedRoomId, String, AiTurnContent, bool)> = Vec::new();
    let mut needs_timer = false;
    with_a2app(|state| {
        for (room_id, info) in state.ai_rooms.iter_mut() {
            // Keep the timer armed while a closed turn still has final writes
            // outstanding, so its failsafe deadline can fire on an idle app.
            if info.final_writes_pending_revoke {
                needs_timer = true;
            }
            if info.ai_turn_in_flight {
                needs_timer = true;
                continue;
            }
            if info.pending_ai_turns.is_empty() {
                continue;
            }
            // A new turn's first snapshot is the card's anchor: post it
            // immediately so the tool-call box appears the moment the agent
            // starts thinking or calls its first tool, even if the previous
            // turn wrote a moment ago. Later snapshots still respect the
            // spacing, which is what keeps a busy turn under the server's
            // state-event rate limit.
            let anchor_pending = info
                .pending_ai_turns
                .front()
                .is_some_and(|(key, _)| info.first_posted_turn.as_deref() != Some(key.as_str()));
            let since_last = info.last_ai_turn_post.map(|last| now.duration_since(last));
            if !turn_post_is_cooled_down(anchor_pending, since_last, info.ai_turn_backoff) {
                needs_timer = true;
                continue;
            }
            if let Some((key, mut content)) = info.pending_ai_turns.pop_front() {
                // The first snapshot of a turn to be WRITTEN is the card's
                // anchor row, whatever the snapshot said when it was queued
                // (a coalesced burst may have replaced the minting one).
                let first = info.first_posted_turn.as_deref() != Some(key.as_str());
                content.first = Some(first);
                if first {
                    info.first_posted_turn = Some(key.clone());
                    info.anchor_in_flight = Some(key.clone());
                }
                // This is the closed turn's final snapshot: consume the
                // marker so its write result is matched to the final tally.
                let final_turn = info.pending_final_turns.remove(&key);
                info.ai_turn_in_flight = true;
                info.last_ai_turn_post = Some(now);
                to_post.push((room_id.clone(), key, content, final_turn));
                // Keep the timer armed so the follow-up (the latest snapshot
                // coalesced while this write was in flight) is not stranded.
                needs_timer = true;
            }
        }
        if needs_timer {
            if state.ai_turn_flush_timer.is_none() {
                state.ai_turn_flush_timer = Some(cx.start_interval(AI_TURN_POST_MIN_INTERVAL.as_secs_f64()));
            }
        } else if let Some(timer) = state.ai_turn_flush_timer.take() {
            cx.stop_timer(timer);
        }
    });
    for (room_id, key, content, final_turn) in to_post {
        let token = post_ai_state_event(&room_id, AI_TURN_EVENT_TYPE, &key, &content);
        if final_turn {
            match token {
                Some(token) => register_final_write(&room_id, token),
                // The final snapshot could not be queued; it is not outstanding.
                None => settle_closed_turn(&room_id),
            }
        }
    }
}

/// Serializes one AI-session activity/tool-call row and hands it to the
/// async worker as a [`AiRoomRequest::PostAiStateEvent`]. Best-effort: the
/// worker logs failures and the turn continues (the reply's receipts are the
/// durable record). Returns the host write id when the write was queued; a
/// dropped write gets no result, so callers relying on one must settle it.
#[cfg(unix)]
fn post_ai_state_event(
    room_id: &OwnedRoomId,
    event_type: &str,
    state_key: &str,
    content: &impl serde::Serialize,
) -> Option<u64> {
    if !room_policy_allows(room_id.as_str(), RoomAccess::Write) {
        with_a2app(|state| {
            if let Some(info) = state.ai_rooms.get_mut(room_id) {
                info.ai_turn_in_flight = false;
                info.ai_turn_write_id = None;
                info.pending_ai_turns.clear();
                info.pending_final_turns.clear();
                info.anchor_in_flight = None;
            }
        });
        return None;
    }
    let flow_context = match super::information_flow::agent_context(room_id.as_str()).and_then(|context| {
        super::information_flow::ensure_room_output(&context, room_id.as_str())?;
        Ok(context)
    }) {
        Ok(context) => context,
        Err(_) => {
            with_a2app(|state| {
                if let Some(info) = state.ai_rooms.get_mut(room_id) {
                    info.ai_turn_in_flight = false;
                    info.ai_turn_write_id = None;
                    info.pending_ai_turns.clear();
                    info.pending_final_turns.clear();
                    info.anchor_in_flight = None;
                }
            });
            return None;
        }
    };
    let json = match serde_json::to_value(content) {
        Ok(json) => json,
        Err(e) => {
            log!("AI Rooms: couldn't serialize an {event_type} state row: {e}");
            if event_type == AI_TURN_EVENT_TYPE {
                with_a2app(|state| {
                    if let Some(info) = state.ai_rooms.get_mut(room_id) {
                        info.ai_turn_in_flight = false;
                        info.ai_turn_write_id = None;
                        if let Some(key) = info.anchor_in_flight.take()
                            && info.first_posted_turn.as_deref() == Some(key.as_str())
                        {
                            info.first_posted_turn = None;
                        }
                    }
                });
            }
            return None;
        }
    };
    let write_id = NEXT_AI_WRITE_ID.fetch_add(1, Ordering::Relaxed);
    if event_type == AI_TURN_EVENT_TYPE {
        with_a2app(|state| {
            if let Some(info) = state.ai_rooms.get_mut(room_id) {
                info.ai_turn_write_id = Some(write_id);
            }
        });
    }
    submit_async_request(MatrixRequest::AiRoom(AiRoomRequest::PostAiStateEvent {
        room_id: room_id.clone(),
        event_type: event_type.to_string(),
        state_key: state_key.to_string(),
        content: json,
        write_id,
        flow_epoch: a2app_core::information_flow::context_epoch(&flow_context).unwrap_or(0),
        flow_context,
    }));
    Some(write_id)
}

/// Writes `text` as an `ai_reply` state event — the agent's only output
/// channel (never `m.room.message`, so it can't loop back into the
/// forwarder as input). This is how both a completed turn's natural reply
/// and the `send_message` tool call reach the room. The turn's pending tool
/// receipts are consumed onto this one card.
#[cfg(unix)]
fn post_ai_reply(room_id: &OwnedRoomId, text: String, answer_id: Option<u64>) {
    let flow_context = super::information_flow::agent_context(room_id.as_str()).and_then(|context| {
        super::information_flow::ensure_room_output(&context, room_id.as_str())?;
        Ok(context)
    });
    if flow_context.is_err() || !room_policy_allows(room_id.as_str(), RoomAccess::Write) {
        let reason = match &flow_context {
            Err(error) => error.clone(),
            Ok(_) => String::from("writing to this room is blocked by its Mini Apps policy"),
        };
        log!("AI Rooms: refusing to post an ai_reply to {room_id}: {reason}");
        // The reply could not even be queued. A natural reply has no worker
        // result to release a token. Settle the other final writes; a
        // parked `send_message` is answered below and its turn keeps them.
        if answer_id.is_none() {
            settle_closed_turn(room_id);
        }
        if let Some(id) = answer_id
            && let Some((_, _, answer, _)) = with_a2app(|state| state.ai_replies.remove(&id)).flatten()
        {
            let _ = answer.send(Err("Writing to this room is blocked in Mini Apps permissions.".into()));
        }
        return;
    }
    // Best-effort traceability, not a precise per-turn link: under queueing,
    // a reply may technically answer an earlier message than the most
    // recently forwarded one.
    let in_reply_to = with_a2app(|state| {
        state.ai_rooms.get(room_id).and_then(|info| info.cursor.as_ref()).map(ToString::to_string)
    }).flatten();
    let tool_calls = with_a2app(|state| {
        let taken = state
            .ai_rooms
            .get_mut(room_id)
            .map(|info| std::mem::take(&mut info.pending_tool_calls))
            .unwrap_or_default();
        // Kept with the parked call so a refused write can hand them back.
        if let Some(id) = answer_id
            && let Some(pending) = state.ai_replies.get_mut(&id)
        {
            pending.3 = taken.clone();
        }
        taken
    })
    .unwrap_or_default();
    log!("AI Rooms: posting ai_reply to room {room_id} (in_reply_to: {in_reply_to:?}).");
    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let write_id = NEXT_AI_WRITE_ID.fetch_add(1, Ordering::Relaxed);
    submit_async_request(MatrixRequest::AiRoom(AiRoomRequest::PostReply {
        room_id: room_id.clone(),
        answer_id,
        write_id,
        flow_epoch: flow_context.as_ref().ok().and_then(|context| a2app_core::information_flow::context_epoch(context).ok()).unwrap_or(0),
        flow_context: flow_context.expect("checked above"),
        content: AiReplyContent {
            v: 1,
            text: text.clone(),
            // Markdown→HTML, so a user mention or a permalink the agent put
            // in its reply renders as a clickable pill (see
            // `ai_room_events::agent_reply_formatted_html`).
            formatted: crate::a2app::ai_room_events::agent_reply_formatted_html(&text),
            tool_calls,
            model: None,
            created_at,
            in_reply_to,
        },
    }));
    // A natural reply is the closed turn's final output: keep its grants until
    // the write result lands (see [`AiRoomAction::PostReplyResult`]). A parked
    // `send_message` reply belongs to a still-live turn and is not counted.
    if answer_id.is_none() {
        register_final_write(room_id, write_id);
    }
}

#[cfg(test)]
mod permission_tests {
    use super::*;

    #[test]
    fn foreground_launches_allow_account_utilities_and_preserve_context_constraints() {
        let mut utility = builtin::stock("account").unwrap();
        utility.permissions.clear();
        utility.capabilities.clear();
        assert!(validate_app_launch_context(&utility, None, false).is_ok());
        assert!(validate_app_launch_context(&utility, Some("!first:test"), false).is_ok());
        assert!(validate_app_launch_context(&utility, Some("!second:test"), false).is_ok());
        assert!(validate_app_launch_context(&utility, Some("!space:test"), true).is_ok());

        utility.scope = A2AppScope::Room { room_id: "!bound:test".into() };
        assert!(validate_app_launch_context(&utility, None, false).is_err());
        assert!(validate_app_launch_context(&utility, Some("!other:test"), false).is_err());
        assert!(validate_app_launch_context(&utility, Some("!bound:test"), false).is_ok());

        let room_app = builtin::stock("room-peek").unwrap();
        assert!(validate_app_launch_context(&room_app, None, false).is_err());
        assert!(validate_app_launch_context(&room_app, Some("!room:test"), false).is_ok());
        assert!(validate_app_launch_context(&room_app, Some("!space:test"), true).is_err());
        let mut space_app = builtin::stock("spaces").unwrap();
        assert!(validate_app_launch_context(&space_app, Some("!room:test"), false).is_err());
        assert!(validate_app_launch_context(&space_app, Some("!space:test"), true).is_ok());
        space_app.scope = A2AppScope::Room { room_id: "!space:test".into() };
        assert!(validate_app_launch_context(&space_app, None, true).is_err());
        assert!(validate_app_launch_context(&space_app, Some("!other:test"), true).is_err());
        assert!(validate_app_launch_context(&space_app, Some("!space:test"), true).is_ok());
    }

    #[test]
    fn public_launch_rejects_bound_and_attached_room_apps_before_changing_instance_state() {
        let previous = A2APP.with(|state| state.replace(None));
        let mut bound_utility = builtin::stock("account").unwrap();
        bound_utility.scope = A2AppScope::Room { room_id: "!bound:test".into() };
        let mut bound_space = builtin::stock("spaces").unwrap();
        bound_space.scope = A2AppScope::Room { room_id: "!space:test".into() };
        let room_app = builtin::stock("room-peek").unwrap();
        let mut cx = Cx::new(Box::new(|_, _| {}));
        for manifest in [bound_utility, bound_space, room_app] {
            initialize_background_test(manifest.clone());
            let dismissal = (manifest.id.clone(), Permission::Network);
            with_a2app(|state| { state.dismissed_prompts.insert(dismissal.clone()); });
            apply_op(&mut cx, &WidgetRef::empty(), A2AppOp::OpenPublicApp(manifest.id.clone()));
            assert!(with_a2app(|state| state.dismissed_prompts.contains(&dismissal)).unwrap(),
                "a rejected public launch must not reset permission choices or replace an existing instance");
            assert_eq!(with_a2app(|state| state.registry.get(&manifest.id).unwrap().scope.clone()), Some(manifest.scope));
        }
        A2APP.with(|state| { state.replace(previous); });
    }

    #[test]
    fn popup_scopes_follow_consumed_room_and_space_targets() {
        let room_read = a2app_core::capabilities::by_id("matrix.room.messages.read").unwrap();
        let extras = serde_json::json!({ "rooms": [TARGET], "space_id": SPACE, "room_id": TARGET });
        assert_eq!(capability_scope(room_read, &extras, Some(SOURCE)), RoomScope::room(SOURCE),
            "unused arguments must not replace the actual attached-room target");
        let space_rooms = a2app_core::capabilities::by_id("matrix.space.rooms.list").unwrap();
        assert_eq!(capability_scope(space_rooms, &serde_json::json!({ "space_id": SPACE }), Some(SPACE)),
            RoomScope::Selection { rooms: Vec::new(), spaces: vec![SPACE.into()] });
        let search = a2app_core::capabilities::by_id("matrix.rooms.messages.search").unwrap();
        assert_eq!(capability_scope(search, &serde_json::json!({ "room_ids": [SOURCE, TARGET] }), Some(SOURCE)),
            RoomScope::Selection { rooms: vec![SOURCE.into(), TARGET.into()], spaces: Vec::new() });
        assert_eq!(capability_scope(search, &serde_json::json!({ "query": "hello" }), Some(SOURCE)), RoomScope::AllRooms);
    }

    #[test]
    fn network_permission_changes_keep_the_public_activation_and_pending_callback() {
        use a2app_core::information_flow as flow;
        use makepad_widgets::widget_async::CxSplashVmExt;
        let previous_account = super::super::information_flow::TEST_ACCOUNT.with(|account|
            account.replace(Some("@network-lifecycle:test".into())));
        let previous_state = A2APP.with(|state| state.replace(None));
        let mut manifest = builtin::stock("public-web").unwrap();
        manifest.id = "runtime-public-network-lifecycle".into();
        manifest.source = format!("fn on_permissions_changed(caps) {{ ui.status.set_text(\"Permissions updated.\") }}\n{}", manifest.source);
        // This test app has host-recorded public code, like a generated app
        // whose source contained no private data.
        flow::add_code_influences(&manifest.id, [flow::Influence::MiniApp { account: "@network-lifecycle:test".into(), app: "public-test-source".into() }]).unwrap();
        initialize_background_test(manifest.clone());
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let template = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            makepad_code_editor::script_mod(vm);
            crate::shared::script_mod(vm);
            super::super::host_set::script_mod(vm);
            let value = script_eval!(vm, { mod.widgets.MiniAppHost {} });
            vm.bx.heap.new_object_ref(value.as_object().unwrap())
        });
        instances::set_host_template(template);
        let key = (manifest.id.clone(), None);
        let host = instances::ensure_public(&mut cx, &manifest, &[]).unwrap();
        makepad_widgets::widget_tree::set_ui_root(&mut cx, &host);
        host.widget(&cx, ids!(status));
        host.widget(&cx, ids!(result));
        let heap = instances::heap_of(&key).unwrap();
        let context = instances::context_of_key(&key).unwrap();
        let epoch = flow::context_epoch(&context).unwrap();
        let lifetime = instances::lifetime_of_heap(heap).unwrap();
        let splash = host.widget(&cx, ids!(splash));
        assert!(splash.borrow_mut::<Splash>().unwrap().call_script_fn(&mut cx, id!(fetch_example), &[]));
        cx.with_vm_and_async(|_| {});
        let request = makepad_widgets::splash_host::take_splash_host_requests().into_iter()
            .find(|request| request.heap_key == heap && request.service == "network.http").unwrap();
        with_a2app(|state| { state.permissions.set(&manifest.id, Permission::Network, GrantState::Granted); });
        apply_permission_to_running(&mut cx, &WidgetRef::empty(), &manifest.id, Permission::Network);
        cx.with_vm_and_async(|_| {});
        assert_eq!(instances::heap_of(&key), Some(heap));
        assert_eq!(flow::context_epoch(&context).unwrap(), epoch);
        assert!(lifetime.load(std::sync::atomic::Ordering::Acquire));
        assert!(std::sync::Arc::ptr_eq(&lifetime, &instances::lifetime_of_heap(heap).unwrap()));
        assert_eq!(instances::host_of(&key).unwrap(), host);
        assert_eq!(host.widget(&cx, ids!(status)).text(), "Permissions updated.");
        let source = splash.borrow::<Splash>().unwrap().view.source.clone();
        let vm_id = cx.script_ref_vm_id(&source).unwrap();
        cx.with_script_vm_id(vm_id, |vm| {
            assert!(vm.cx().script_data.std.host_io_only(), "granting network must retain the host-only I/O guard");
            assert!(vm.cx().script_data.std.net.is_none(), "native networking stays disabled");
        });
        assert_eq!(makepad_widgets::splash_host::splash_host_respond(&mut cx, heap, request.req_id,
            Ok(r#"{"status":200,"body":"Public callback survived","headers":{}}"#)),
            makepad_widgets::splash_host::SplashRespondOutcome::Delivered);
        cx.with_vm_and_async(|_| {});
        assert_eq!(host.widget(&cx, ids!(result)).text(), "Public callback survived");
        instances::quit_app(&mut cx, &manifest.id);
        instances::clear_host_template();
        A2APP.with(|state| { state.replace(previous_state); });
        super::super::information_flow::TEST_ACCOUNT.with(|account| { account.replace(previous_account); });
        makepad_widgets::splash_host::take_splash_host_requests();
    }

    #[test]
    fn related_requests_share_a_dialog_and_stale_group_answers_cannot_grant_the_next() {
        let previous = A2APP.with(|state| state.replace(None));
        let manifest = builtin::stock("room-peek").unwrap();
        initialize_background_test(manifest.clone());
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let ui = WidgetRef::empty();
        let activation = a2app_core::information_flow::ContextId::App {
            account: "group-fixture".into(), app: manifest.id.clone(), room: Some(SOURCE.into()),
        };
        let ids = [next_permission_prompt_id(), next_permission_prompt_id()];
        with_a2app(|state| {
            for (id, perm) in ids.into_iter().zip([Permission::MatrixRoomRead, Permission::MatrixRoomWatch]) {
                state.prompts.push_back(PermissionPrompt { setup: None, id, subject: manifest.id.clone(), perm,
                    parked: vec![ParkedRequest::Bridge(None)], tool: None, flow: None, enable_writes: false,
                    activations: vec![(0, id, activation.clone(), 1)],
                });
            }
        });
        show_next_permission_prompt(&mut cx, &ui);
        with_a2app(|state| {
            assert!(state.prompts.is_empty());
            assert_eq!(state.active_prompt.as_ref().unwrap().id, ids[0]);
            assert_eq!(state.active_prompt_batch.len(), 1);
        });
        let response = PermissionPromptGroupResponse { group_id: ids[0], responses: ids.iter().map(|id|
            PermissionPromptResponse { prompt_id: *id, answer: PermissionPromptAction::AllowScoped {
                scope: RoomScope::room(SOURCE), duration: GrantDuration::RobrixSession, network: None,
            } }).collect() };
        // A singleton action must never answer one member of a grouped dialog.
        answer_permission_prompt(&mut cx, &ui, response.responses[0].clone());
        with_a2app(|state| assert!(state.permissions.scoped_grants(&manifest.id).is_empty()));
        permission_batch::answer_group(&mut cx, &ui, response.clone());
        with_a2app(|state| {
            assert!(state.active_prompt.is_none());
            assert!(state.active_prompt_batch.is_empty());
            let context = PermissionContext { origin_room: Some(SOURCE), target_room: Some(SOURCE) };
            for capability in ["matrix.room.messages.read", "on_room_message"] {
                assert_eq!(state.permissions.effective_capability_in_context(&manifest,
                    a2app_core::capabilities::by_id(capability).unwrap(), context), Effective::Granted);
            }
            assert_eq!(state.permissions.effective_capability_in_context(&manifest,
                a2app_core::capabilities::by_id("matrix.room.messages.read").unwrap(),
                PermissionContext { origin_room: Some(TARGET), target_room: Some(TARGET) }), Effective::NeedsPrompt);
        });
        queue_permission_prompt(&mut cx, &ui, manifest.id.clone(), Permission::MatrixRoomWatch,
            ParkedRequest::Bridge(None), None);
        let next = with_a2app(|state| state.active_prompt.as_ref().unwrap().id).unwrap();
        permission_batch::answer_group(&mut cx, &ui, response);
        with_a2app(|state| assert_eq!(state.active_prompt.as_ref().unwrap().id, next));
        A2APP.with(|state| { state.replace(previous); });
    }

    #[test]
    fn installing_stock_retires_source_inspection_before_recovering_empty_storage() {
        use a2app_core::information_flow as flow;
        let stock = builtin::stock("room-info").unwrap();
        let previous_account = super::super::information_flow::TEST_ACCOUNT.with(|account|
            account.replace(Some("@stock-install:test".into())));
        let previous_state = A2APP.with(|state| state.replace(None));
        initialize_background_test(stock.clone());
        let context = super::super::information_flow::record_source_edit(&stock).unwrap();
        assert!(flow::context_epoch(&context).is_ok());
        assert!(!flow::labels(&context).unwrap().is_empty());
        let mut cx = Cx::new(Box::new(|_, _| {}));
        install_version(&mut cx, &WidgetRef::empty(), stock.clone(), "Restored built-in".into());
        let stopped = flow::context_epoch(&context).is_err();
        flow::register_context(&context).unwrap();
        let label = flow::labels(&context).unwrap();
        let history = flow::code_labels(&stock.id).unwrap();
        flow::remove_context(&context).unwrap();
        A2APP.with(|state| { state.replace(previous_state); });
        super::super::information_flow::TEST_ACCOUNT.with(|account| { account.replace(previous_account); });
        assert!(stopped, "metadata inspection must not keep a retired stock activation alive");
        assert!(label.is_empty());
        assert!(history.contains(&flow::Source::Account { account: "@stock-install:test".into() }));
    }

    const SOURCE: &str = "!source:example.org";
    const TARGET: &str = "!target:example.org";
    const SPACE: &str = "!space:example.org";

    #[test]
    fn adopting_the_complete_builtin_default_clears_its_pending_update() {
        let stock = builtin::stock("roll-call").unwrap();
        let mut customized = stock.clone();
        customized.name = "My dice roller".into();
        initialize_background_test(customized.clone());
        with_a2app(|state| {
            state.persisted.builtin_updates.insert(stock.id.clone(), "new-default".into());
            state.clear_adopted_builtin_update(&customized);
            assert!(state.persisted.builtin_updates.contains_key(&stock.id),
                "matching code alone must not discard a metadata update");
            let mut adopted = stock.clone();
            adopted.scope = A2AppScope::Room { room_id: SOURCE.into() };
            state.clear_adopted_builtin_update(&adopted);
            assert!(!state.persisted.builtin_updates.contains_key(&stock.id));
            assert!(state.registry_dirty);
        });
    }

    fn bridge(service: &str, args: serde_json::Value, room: Option<&str>) -> ParkedRequest {
        ParkedRequest::Bridge(Some(SplashHostRequest {
            app_tag: a2app_core::manifest::instance_tag("permission-test", room),
            heap_key: 0,
            req_id: 1,
            service: service.to_string(),
            args_json: args.to_string(),
            may_prompt: true,
        }))
    }

    #[test]
    fn retired_bridge_answer_cannot_enable_room_writes_or_create_a_grant() {
        let previous = A2APP.with(|state| state.replace(None));
        struct Restore(Option<A2AppState>);
        impl Drop for Restore {
            fn drop(&mut self) { A2APP.with(|state| { state.replace(self.0.take()); }); }
        }
        let _restore = Restore(previous);
        let manifest = builtin::stock("roll-call").unwrap();
        let subject = manifest.id.clone();
        initialize_background_test(manifest.clone());
        let parked = bridge("matrix.send_message", serde_json::json!({"body":"retired fixture"}), Some(SOURCE));
        let capability = parked_capability(&parked).unwrap();
        let context = PermissionContext { origin_room: Some(SOURCE), target_room: Some(SOURCE) };
        let id = next_permission_prompt_id();
        with_a2app(|state| {
            assert!(services::can_enable_writes(&state.permissions, &manifest, capability, context));
            state.active_prompt = Some(PermissionPrompt { setup: None, id, subject: subject.clone(), perm: Permission::MatrixRoomSend,
                parked: vec![parked], tool: None, flow: None, enable_writes: true,
                activations: vec![(0, 1, a2app_core::information_flow::ContextId::App {
                    account: "retired-fixture".into(), app: subject.clone(), room: Some(SOURCE.into()),
                }, 1)],
            });
            state.perms_dirty = false;
        });
        let mut cx = Cx::new(Box::new(|_, _| {}));
        answer_permission_prompt(&mut cx, &WidgetRef::empty(), PermissionPromptResponse { prompt_id: id,
            answer: PermissionPromptAction::AllowScoped { scope: RoomScope::AllRooms, duration: GrantDuration::RobrixSession, network: None },
        });
        with_a2app(|state| {
            assert!(!state.permissions.matrix_write());
            assert!(state.permissions.room_send_grants(&subject).is_empty());
            assert!(!state.perms_dirty);
            assert!(state.active_prompt.is_none());
            assert!(state.prompts.is_empty());
        });
    }

    #[test]
    fn setup_write_prompt_enables_its_room_grant_and_rechecks_new_denials() {
        use a2app_core::information_flow as flow;
        let previous_account = super::super::information_flow::TEST_ACCOUNT.with(|account|
            account.replace(Some("@write-setup:test".into())));
        let previous_state = A2APP.with(|state| state.replace(None));
        let mut manifest = builtin::stock("roll-call").unwrap();
        manifest.id = "runtime-write-setup".into();
        manifest.capabilities = vec!["matrix.room.message.send".into()];
        manifest.source = r#"
fn setup(){
    host.request("permissions.request", {perm:"matrix-room-send"}, fn(r){
        let answer = "denied"
        if r.is_ok && r.data.granted { answer = "granted" }
        ui.note.set_text(answer)
    })
}
View{note := Label{text:"waiting"}}
"#.into();
        initialize_background_test(manifest.clone());
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let (template, root) = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            makepad_code_editor::script_mod(vm);
            crate::shared::script_mod(vm);
            super::super::host_set::script_mod(vm);
            let template = script_eval!(vm, { mod.widgets.MiniAppHost {} });
            let template = vm.bx.heap.new_object_ref(template.as_object().unwrap());
            let root = script_eval!(vm, { mod.widgets.View {} });
            (template, WidgetRef::script_from_value(vm, root))
        });
        makepad_widgets::widget_tree::set_ui_root(&mut cx, &root);
        instances::set_host_template(template);
        let key = (manifest.id.clone(), Some(SOURCE.try_into().unwrap()));
        let host = instances::ensure(&mut cx, &key, &manifest, &[]).unwrap();
        instances::adopt(&mut cx, &key, root.widget_uid(), instances::Surface::Modal).unwrap();
        host.widget(&cx, ids!(note));
        let context = instances::context_of_key(&key).unwrap();
        let epoch = flow::context_epoch(&context).unwrap();
        let heap = instances::heap_of(&key).unwrap();
        let capability = a2app_core::capabilities::by_id("matrix.room.message.send").unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for blocker in ["none", "room", "app", "capability"] {
            with_a2app(|state| state.permissions = PermissionStore::default());
            assert!(host.widget(&cx, ids!(splash)).borrow_mut::<Splash>().unwrap()
                .call_script_fn(&mut cx, id!(setup), &[]));
            cx.with_vm_and_async(|_| {});
            let request = makepad_widgets::splash_host::take_splash_host_requests().into_iter()
                .find(|request| request.heap_key == heap && request.service == "permissions.request").unwrap();
            let request_id = request.req_id;
            let parked = ParkedRequest::Bridge(Some(request));
            let id = next_permission_prompt_id();
            with_a2app(|state| {
                assert!(parked_can_enable_writes(state, &manifest.id, Permission::MatrixRoomSend, &parked));
                state.active_prompt = Some(PermissionPrompt { setup: None, id, subject: manifest.id.clone(), perm: Permission::MatrixRoomSend,
                    parked: vec![parked], tool: None, flow: None, enable_writes: true,
                    activations: vec![(heap, request_id, context.clone(), epoch)],
                });
                match blocker {
                    "room" => state.permissions.set_room_policy(SOURCE, RoomAccess::Write, PolicyDecision::Deny),
                    "app" => state.permissions.set(&manifest.id, Permission::MatrixRoomSend, GrantState::Denied),
                    "capability" => state.permissions.set_capability(&manifest.id, capability.id, GrantState::Denied),
                    _ => {},
                }
            });
            answer_permission_prompt(&mut cx, &WidgetRef::empty(), PermissionPromptResponse { prompt_id: id,
                answer: PermissionPromptAction::AllowScoped { scope: RoomScope::room(SOURCE), duration: GrantDuration::RobrixSession, network: None },
            });
            cx.with_vm_and_async(|_| {});
            with_a2app(|state| {
                let context = PermissionContext { origin_room: Some(SOURCE), target_room: Some(SOURCE) };
                if blocker == "none" {
                    assert!(state.permissions.matrix_write());
                    assert_eq!(state.permissions.effective_capability_in_context(&manifest, capability, context), Effective::Granted);
                    assert_eq!(state.permissions.effective_capability_in_context(&manifest, capability,
                        PermissionContext { target_room: Some(TARGET), ..context }), Effective::NeedsPrompt);
                } else {
                    assert!(!state.permissions.matrix_write(), "{blocker} denial must prevent enabling room changes");
                    assert!(state.permissions.room_send_grants(&manifest.id).is_empty());
                    assert_eq!(state.permissions.effective_capability_in_context(&manifest, capability, context), Effective::Denied);
                }
                assert!(state.active_prompt.is_none());
                assert!(state.prompts.is_empty());
            });
            assert_eq!(host.widget(&cx, ids!(note)).text(), if blocker == "none" {"granted"} else {"denied"});
            assert_eq!(instances::heap_of(&key), Some(heap), "setup keeps the original callback activation");
        }
        }));
        instances::quit_app(&mut cx, &manifest.id);
        instances::clear_host_template();
        A2APP.with(|state| { state.replace(previous_state); });
        super::super::information_flow::TEST_ACCOUNT.with(|account| { account.replace(previous_account); });
        makepad_widgets::splash_host::take_splash_host_requests();
        if let Err(error) = result { std::panic::resume_unwind(error); }
    }

    #[test]
    fn data_arriving_during_review_renews_the_dialog_and_preserves_the_worker() {
        use a2app_core::information_flow as flow;
        use std::{future::Future, task::{Context, Poll, Waker}};
        let _queue_guard = super::super::effect_review::TEST_LOCK.lock().unwrap();
        let previous_account = super::super::information_flow::TEST_ACCOUNT.with(|account|
            account.replace(Some("@renewed-review:test".into())));
        let previous_state = A2APP.with(|state| state.replace(None));
        let mut manifest = builtin::stock("roll-call").unwrap();
        manifest.id = "runtime-renewed-flow-review".into();
        manifest.permissions.push(Permission::ClipboardWrite.as_str().into());
        flow::add_code_influences(&manifest.id, [flow::Influence::MiniApp { account: "@renewed-review:test".into(), app: "fixture".into() }]).unwrap();
        initialize_background_test(manifest.clone());
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let (template, root) = cx.with_vm(|vm| {
            makepad_widgets::script_mod(vm);
            makepad_code_editor::script_mod(vm);
            crate::shared::script_mod(vm);
            super::super::host_set::script_mod(vm);
            let template = script_eval!(vm, { mod.widgets.MiniAppHost {} });
            let template = vm.bx.heap.new_object_ref(template.as_object().unwrap());
            let root = script_eval!(vm, { mod.widgets.View {} });
            (template, WidgetRef::script_from_value(vm, root))
        });
        makepad_widgets::widget_tree::set_ui_root(&mut cx, &root);
        instances::set_host_template(template);
        let key = (manifest.id.clone(), Some(SOURCE.try_into().unwrap()));
        instances::ensure(&mut cx, &key, &manifest, &[]).unwrap();
        instances::adopt(&mut cx, &key, root.widget_uid(), instances::Surface::Modal).unwrap();
        let context = instances::context_of_key(&key).unwrap();
        let epoch = flow::context_epoch(&context).unwrap();
        let recipient = flow::Recipient::Clipboard;
        let action = flow::SensitiveAction { kind: "device.clipboard.write".into(), target: "clipboard".into() };
        let payload = serde_json::json!({"text":"immutable reviewed contents"});
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            flow::add_sources(&context, [flow::Source::Account { account: context.account().into() }]).unwrap();
            let capture = flow::prepare_effect_for_activation(&context, epoch, Some(&recipient), Some(&action), &payload).unwrap();
            let mut waiter = Box::pin(super::super::effect_review::request(capture.clone(), true));
            let mut task = Context::from_waker(Waker::noop());
            assert!(waiter.as_mut().poll(&mut task).is_pending());
            let worker = super::super::effect_review::take_pending().pop().unwrap();
            let old_id = next_permission_prompt_id();
            with_a2app(|state| state.active_prompt = Some(PermissionPrompt { setup: None, id: old_id, subject: manifest.id.clone(), perm: Permission::ClipboardWrite,
                parked: Vec::new(), tool: None, flow: Some(FlowContinuation::Worker(worker)), enable_writes: false, activations: Vec::new(),
            }));
            let arrived = flow::Source::Room { account: context.account().into(), room: TARGET.into() };
            flow::add_sources(&context, [arrived.clone()]).unwrap();
            answer_permission_prompt(&mut cx, &WidgetRef::empty(), PermissionPromptResponse { prompt_id: old_id, answer: PermissionPromptAction::AllowFlowOnce });
            let renewed_id = with_a2app(|state| {
                let prompt = state.active_prompt.as_ref().unwrap();
                assert_ne!(prompt.id, old_id);
                let review = prompt.flow.as_ref().unwrap().review();
                assert_ne!(review.id, capture.id);
                assert!(review.sources.contains(&arrived));
                assert_eq!(review.payload, capture.payload);
                prompt.id
            }).unwrap();
            assert!(waiter.as_mut().poll(&mut task).is_pending(), "new data must renew review without failing the original worker");
            answer_permission_prompt(&mut cx, &WidgetRef::empty(), PermissionPromptResponse { prompt_id: old_id, answer: PermissionPromptAction::AllowFlowSession });
            assert!(waiter.as_mut().poll(&mut task).is_pending(), "an answer from the old dialog cannot approve the renewed one");
            assert!(flow::check_effect_for_activation(&context, epoch, Some(&recipient), Some(&action), &payload).is_err());
            answer_permission_prompt(&mut cx, &WidgetRef::empty(), PermissionPromptResponse { prompt_id: renewed_id, answer: PermissionPromptAction::AllowFlowOnce });
            assert!(matches!(waiter.as_mut().poll(&mut task), Poll::Ready(Ok(()))));
            flow::commit_effect_for_activation(&context, epoch, Some(&recipient), Some(&action), &payload).unwrap();
            assert!(flow::commit_effect_for_activation(&context, epoch, Some(&recipient), Some(&action), &payload).is_err());
        }));
        instances::quit_app(&mut cx, &manifest.id);
        instances::clear_host_template();
        A2APP.with(|state| { state.replace(previous_state); });
        super::super::information_flow::TEST_ACCOUNT.with(|account| { account.replace(previous_account); });
        makepad_widgets::splash_host::take_splash_host_requests();
        if let Err(error) = result { std::panic::resume_unwind(error); }
    }

    #[test]
    fn version_commit_records_editor_acquisition_and_parent() {
        let mut manifest = builtin::stock("room-peek").unwrap();
        manifest.id = "runtime-provenance-version".into();
        manifest.builtin = false;
        let actor = VersionActor { user_id: "@editor:example.org".into(), display_name: Some("Editor".into()) };
        let source = AcquisitionSource::File { file_name: "app.splashapp".into(), path: Some("/Downloads/app.splashapp".into()) };
        commit_version(&mut manifest, VersionOrigin::Import, "Imported", Some(actor.clone()), Some(source.clone())).unwrap();
        let original = manifest.current_version.clone().unwrap();
        manifest.source.push_str("\n// edited\n");
        commit_version(&mut manifest, VersionOrigin::Manual, "Changed by hand", Some(actor.clone()), None).unwrap();
        let (edited, code) = persistence::load_version(&manifest.id, manifest.current_version.as_deref().unwrap()).unwrap();
        assert_eq!(edited.parent.as_deref(), Some(original.as_str()));
        assert_eq!(edited.actor, Some(actor.clone()));
        assert!(!edited.imported);
        assert!(edited.at_unix > 0);
        assert_eq!(edited.host_version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
        assert_eq!(edited.host_revision.as_deref(), Some(env!("ROBRIX_GIT_COMMIT_HASH")).filter(|revision| !revision.is_empty()));
        assert_eq!(code, manifest.source);
        let (imported, original_code) = persistence::load_version(&manifest.id, &original).unwrap();
        assert_eq!(imported.acquired_from, Some(source));
        let mut restored = imported.apply_to(&manifest, original_code);
        commit_version(&mut restored, VersionOrigin::Restore, "Restored original", Some(actor), None).unwrap();
        let (restoration, _) = persistence::load_version(&manifest.id, restored.current_version.as_deref().unwrap()).unwrap();
        assert_eq!(restoration.origin, VersionOrigin::Restore);
        assert_eq!(restoration.parent.as_deref(), Some(original.as_str()));
        assert_eq!(persistence::list_versions(&manifest.id).len(), 3);
        persistence::remove_user_app(&manifest.id);
    }

    #[test]
    fn room_import_index_accepts_only_local_acquisitions() {
        let manifest = builtin::stock("room-peek").unwrap();
        let mut version = versions::new_version(&manifest, VersionOrigin::Import, "Added", None, 123, 0);
        version.actor = Some(VersionActor { user_id: "@importer:example.org".into(), display_name: None });
        version.acquired_from = Some(AcquisitionSource::RoomAttachment {
            room_id: "!room:example.org".into(), event_id: Some("$event:example.org".into()),
            media_uri: Some("mxc://example.org/file".into()), shared_at_unix: Some(100), file_name: "app.splashapp".into(),
            sender: Some(VersionActor { user_id: "@sender:example.org".into(), display_name: None }),
        });
        let mut index = HashMap::new();
        version.imported = true;
        index_room_import(&mut index, "shared-copy", &version);
        assert!(index.is_empty(), "shared claims cannot mark a file as locally imported");
        version.imported = false;
        index_room_import(&mut index, "local-copy", &version);
        assert_eq!(index.len(), 2);
        assert_eq!(index.get(&("@importer:example.org".into(), "!room:example.org".into(), "mxc://example.org/file".into())), Some(&"local-copy".into()));
        assert!(!index.keys().any(|key| key.0 == "@sender:example.org"));

        let mut local = manifest.clone();
        local.id = "runtime-room-import-restart".into();
        local.builtin = false;
        local.current_version = Some(persistence::append_version(&local, version).unwrap());
        persistence::save_user_app(&local).unwrap();
        let registry = AppRegistry::new(vec![local.clone()]);
        let rebuilt = index_room_imports(&registry);
        assert_eq!(rebuilt.get(&("@importer:example.org".into(), "!room:example.org".into(), "$event:example.org".into())), Some(&local.id));
        assert_eq!(rebuilt.get(&("@importer:example.org".into(), "!room:example.org".into(), "mxc://example.org/file".into())), Some(&local.id));
        persistence::remove_user_app(&local.id);
    }

    #[test]
    fn completed_generation_retry_uses_saved_source_and_original_activation() {
        use a2app_core::information_flow as flow;
        let path = std::env::temp_dir().join(format!("robrix-generation-review-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let mut registry = flow::Registry::open(&path).unwrap();
        let context = flow::ContextId::Agent { account: "alice".into(), room: SOURCE.into() };
        registry.register_context(&context).unwrap();
        registry.add_influences(&context, [flow::Influence::Model("provider".into())]).unwrap();
        let mut pending = PendingGeneratedApp {
            manifest: Box::new(a2app_core::builtin::stock("room-peek").unwrap()), refine_of: None,
            context: context.clone(), epoch: registry.context_epoch(&context).unwrap(), request: "build fixture".into(), attribution: None,
            action_target: Some(SOURCE.into()), review_id: None, effect_review_id: None,
        };
        assert!(pending.can_retain());
        let source = pending.manifest.source.clone();
        assert!(pending.commit_review(|context, epoch, action, payload| registry.commit_exact_action_for_activation(context, epoch, action, payload)).is_err());
        let decision = registry.recent_action_decisions().unwrap().pop().unwrap();
        let capture = decision.request.unwrap();
        assert_eq!(serde_json::from_str::<serde_json::Value>(&capture.payload).unwrap()["source"], source);
        registry.grant_exact_action_for_activation(&context, capture.id, &decision.influences, pending.epoch).unwrap();
        pending.manifest.source.push_str("\nchanged after review");
        assert!(pending.commit_review(|context, epoch, action, payload| registry.commit_exact_action_for_activation(context, epoch, action, payload)).is_err());
        pending.manifest.source = source;
        pending.commit_review(|context, epoch, action, payload| registry.commit_exact_action_for_activation(context, epoch, action, payload)).unwrap();
        assert!(pending.commit_review(|context, epoch, action, payload| registry.commit_exact_action_for_activation(context, epoch, action, payload)).is_err());
        let decision = registry.recent_action_decisions().unwrap().pop().unwrap();
        registry.grant_exact_action_for_activation(&context, decision.request.unwrap().id, &decision.influences, pending.epoch).unwrap();
        registry.remove_context(&context);
        registry.register_context(&context).unwrap();
        registry.grant_authority(&context, pending.action().unwrap(), flow::AuthoritySession::RobrixSession).unwrap();
        assert!(pending.commit_review(|context, epoch, action, payload| registry.commit_exact_action_for_activation(context, epoch, action, payload)).is_err(),
            "a restarted agent cannot install an old reviewed build");
        pending.manifest.source = "x".repeat(1024 * 1024);
        assert!(!pending.can_retain());
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn attached_bridge_requests_cannot_spoof_the_prompt_target() {
        let args = serde_json::json!({
            "room_id": TARGET, "space_id": SPACE, "origin_room": TARGET,
        });
        for service in ["matrix.read_messages", "matrix.room_members", "matrix.room_info"] {
            let request = bridge(service, args.clone(), Some(SOURCE));
            assert_eq!(parked_rooms(&request), (Some(SOURCE.into()), Some(SOURCE.into())), "{service}");
        }
        let unattached = bridge("matrix.read_messages", args, None);
        assert_eq!(parked_rooms(&unattached), (None, None));
        assert_eq!(parked_rooms(&ParkedRequest::Bridge(None)), (None, None));
    }

    #[test]
    fn cross_room_bridge_requests_show_the_actual_room_or_space() {
        for service in ["matrix.rooms_info", "matrix.rooms_messages", "matrix.rooms_send"] {
            let request = bridge(service, serde_json::json!({ "room_id": format!(" {TARGET} "), "space_id": SPACE }), Some(SOURCE));
            assert_eq!(parked_rooms(&request), (Some(SOURCE.into()), Some(TARGET.into())), "{service}");
        }
        for service in ["matrix.space_info", "matrix.space_rooms"] {
            let request = bridge(service, serde_json::json!({ "space_id": SPACE, "room_id": TARGET }), Some(SOURCE));
            assert_eq!(parked_rooms(&request), (Some(SOURCE.into()), Some(SPACE.into())), "{service}");
        }
        let missing_target = bridge("matrix.rooms_messages", serde_json::json!({ "room_id": " " }), Some(SOURCE));
        assert_eq!(parked_rooms(&missing_target), (Some(SOURCE.into()), None));
    }

    #[test]
    fn prompt_capability_grant_stays_with_that_ability_room_and_session() {
        let request = bridge("matrix.room_members", serde_json::json!({ "room_id": TARGET }), Some(SOURCE));
        let capability = parked_capability(&request).unwrap();
        assert_eq!(capability.id, "matrix.room.members.read");
        let sibling = a2app_core::capabilities::by_id("matrix.room.messages.read").unwrap();
        assert_eq!(capability.group, sibling.group);
        let (origin, target) = parked_rooms(&request);
        let mut permissions = PermissionStore::default();
        permissions.grant_scoped("permission-test", capability.group.unwrap(), Some(capability.id),
            RoomScope::room(target.as_deref().unwrap()), GrantDuration::RoomSession, origin.as_deref()).unwrap();
        let context = PermissionContext { origin_room: origin.as_deref(), target_room: target.as_deref() };
        let verdict = |store: &PermissionStore, cap, context| store.effective_capability_for_in_context(
            "permission-test", |_| true, |_| true, cap, context,
        );
        assert_eq!(verdict(&permissions, capability, context), Effective::Granted);
        assert_eq!(verdict(&permissions, sibling, context), Effective::NeedsPrompt);
        assert_eq!(verdict(&permissions, capability, PermissionContext { target_room: Some(TARGET), ..context }), Effective::NeedsPrompt);
        assert_eq!(verdict(&permissions, capability, PermissionContext { origin_room: Some(TARGET), ..context }), Effective::NeedsPrompt);
        permissions.clear_room_session(SOURCE);
        assert_eq!(verdict(&permissions, capability, context), Effective::NeedsPrompt);
    }

    #[test]
    fn subscriptions_identify_the_hook_while_explicit_group_requests_stay_broad() {
        let hook = bridge("events.subscribe", serde_json::json!({ "event": "on_room_message" }), Some(SOURCE));
        assert_eq!(parked_capability(&hook).unwrap().id, "on_room_message");
        let malformed_hook = bridge("events.subscribe", serde_json::json!({ "event": "not_a_real_hook" }), Some(SOURCE));
        assert!(parked_capability(&malformed_hook).is_none());
        let group = bridge("permissions.request", serde_json::json!({ "permission": "matrix-room-read", "capability": "matrix.room.members.read" }), Some(SOURCE));
        assert!(parked_capability(&group).is_none());
        assert!(parked_capability(&ParkedRequest::Bridge(None)).is_none());
    }

    #[test]
    fn allow_once_requires_a_concrete_request_without_a_subscription() {
        assert!(parked_can_allow_once(&bridge("matrix.read_messages", serde_json::json!({}), Some(SOURCE))));
        for service in ["permissions.request", "events.subscribe"] {
            assert!(!parked_can_allow_once(&bridge(service, serde_json::json!({}), Some(SOURCE))));
        }
        assert!(!parked_can_allow_once(&ParkedRequest::Bridge(None)));
    }

    #[test]
    fn queued_composer_rechecks_revocation_and_hard_room_protection() {
        let mut permissions = PermissionStore::default();
        permissions.set_global_policy(RoomAccess::Write, PolicyDecision::Ask);
        let capability = a2app_core::capabilities::by_id("host.composer.insert").unwrap();
        let grant = permissions.grant_scoped("composer-test", capability.group.unwrap(), Some(capability.id),
            RoomScope::room(TARGET), GrantDuration::RobrixSession, Some(TARGET)).unwrap();
        permissions.mark_request_once(grant);
        let authorization = matrix::policy::MatrixAuthorization::new("composer-test", capability.id, Some(TARGET), &permissions);
        let pending = PendingRoomAction {
            room_id: TARGET.try_into().unwrap(), action: RoomAction::InsertDraft("fixture".into()),
            since: Instant::now(), authorization: Some(authorization), close_after: None,
        };
        assert!(pending.permitted(&permissions));
        permissions.remove_scoped_grant(grant);
        assert!(pending.permitted(&permissions), "consuming Allow once does not cancel its already queued action");
        permissions.set_room_policy(TARGET, RoomAccess::Write, PolicyDecision::Deny);
        assert!(!pending.permitted(&permissions), "hard protection overrides the queued receipt");
        permissions.set_room_policy(TARGET, RoomAccess::Write, PolicyDecision::Ask);
        permissions.set("composer-test", capability.group.unwrap(), GrantState::Denied);
        assert!(!pending.permitted(&permissions), "explicit revocation overrides the queued receipt");
    }

    #[test]
    fn native_room_navigation_does_not_need_an_app_context_but_still_expires() {
        let mut pending = PendingRoomAction {
            room_id: TARGET.try_into().unwrap(), action: RoomAction::JumpToEvent("$event:s".try_into().unwrap()),
            since: Instant::now(), authorization: None, close_after: None,
        };
        assert!(pending.check().is_ok());
        pending.since = Instant::now() - ROOM_ACTION_TTL - Duration::from_secs(1);
        assert!(pending.check().is_err());
    }

    #[test]
    fn replacing_queued_composer_keeps_only_the_same_activation_alive() {
        let context = a2app_core::information_flow::ContextId::App {
            account: "alice".into(), app: "composer-test".into(), room: Some(TARGET.into()),
        };
        let key = ("composer-test".to_string(), Some(TARGET.try_into().unwrap()));
        let pending = |epoch| {
            let mut authorization = matrix::policy::MatrixAuthorization::new("composer-test", "host.composer.insert", Some(TARGET), &PermissionStore::default());
            authorization.flow_context = Some(context.clone());
            authorization.flow_epoch = Some(epoch);
            PendingRoomAction {
                room_id: TARGET.try_into().unwrap(), action: RoomAction::InsertDraft("fixture".into()),
                since: Instant::now(), authorization: Some(authorization), close_after: None,
            }
        };
        let mut previous = pending(1);
        previous.close_after = Some(key.clone());
        let mut replacement = pending(1);
        replacement.inherit_requester(&mut previous);
        assert!(previous.close_after.is_none());
        assert_eq!(replacement.close_after, Some(key.clone()));
        let mut reopened = pending(2);
        reopened.inherit_requester(&mut replacement);
        assert!(reopened.close_after.is_none());
        assert_eq!(replacement.close_after, Some(key));
    }

    #[cfg(unix)]
    #[test]
    fn stopped_session_discards_pending_turn_data_but_keeps_room_enabled() {
        let cursor: OwnedEventId = "$handled:example.org".try_into().unwrap();
        let mut info = AiRoomInfo::new(Some(cursor.clone()));
        info.posted_by_tool_this_turn = true;
        info.status_busy = true;
        info.status_queued = 3;
        info.ai_turn_in_flight = true;
        info.ai_turn_write_id = Some(17);
        info.anchor_in_flight = Some("old-turn".into());
        info.active_turn = Some(ActiveTurn {
            key: "old-turn".into(), tool_calls: Vec::new(), thinking: true, seq: 1, created_at: 1,
        });
        info.pending_tool_calls.push(AiReplyToolCall {
            name: "post_room_message".into(), detail: Some("old private target".into()), ok: false, summary: "waiting".into(),
        });
        info.pending_ai_turns.push_back(("old-turn".into(), AiTurnContent {
            v: 1, turn: "old-turn".into(), first: Some(true), status: AiTurnStatus::Running,
            seq: 1, thinking: true, tool_calls: Vec::new(), created_at: 1,
        }));
        info.discard_session_work();
        assert!(info.active_turn.is_none(), "the timeline must settle the old card locally");
        assert!(info.pending_ai_turns.is_empty(), "a new activation cannot publish old snapshots");
        assert!(info.pending_tool_calls.is_empty());
        assert!(!info.ai_turn_in_flight && info.anchor_in_flight.is_none());
        assert!(info.ai_turn_write_id.is_none());
        assert!(!info.posted_by_tool_this_turn && !info.status_busy && info.status_queued == 0);
        assert!(info.session_on, "the next member prompt may start a fresh session");
        assert_eq!(info.cursor, Some(cursor), "already forwarded messages must not be replayed");
    }

    #[cfg(unix)]
    #[test]
    fn agent_read_write_and_space_prompts_keep_source_and_target_distinct() {
        for (kind, target, capability) in [
            (ReadToolKind::Messages { limit: 10 }, SOURCE, "matrix.room.messages.read"),
            (ReadToolKind::OtherRoom { room: TARGET.into(), limit: 10 }, TARGET, "matrix.rooms.messages.read"),
            (ReadToolKind::SpaceInfo { space: SPACE.into() }, SPACE, "matrix.space.info.read"),
            (ReadToolKind::SpaceRooms { space: SPACE.into() }, SPACE, "matrix.space.rooms.list"),
        ] {
            let (answer, _) = std::sync::mpsc::channel();
            let parked = ParkedRequest::AiTool {
                room_id: SOURCE.try_into().unwrap(),
                job: SessionJob::ReadTool { kind, answer },
            };
            assert_eq!(parked_rooms(&parked), (Some(SOURCE.into()), Some(target.into())));
            assert_eq!(parked_capability(&parked).unwrap().id, capability);
        }
        let (answer, _) = std::sync::mpsc::channel();
        let write = ParkedRequest::AiTool {
            room_id: SOURCE.try_into().unwrap(),
            job: SessionJob::PostRoomMessage { room_id: TARGET.into(), text: "fixture".into(), answer },
        };
        assert_eq!(parked_rooms(&write), (Some(SOURCE.into()), Some(TARGET.into())));
        assert_eq!(parked_capability(&write).unwrap().id, "matrix.rooms.message.send");
    }

    #[cfg(unix)]
    #[test]
    fn directory_defaults_run_once_per_room_and_leave_other_groups_alone() {
        let room: OwnedRoomId = SOURCE.try_into().unwrap();
        let subject = agent_subject(SOURCE);
        // A stored Deny on a directory group is never overridden.
        let mut permissions = PermissionStore::default();
        let denied_group = a2app_core::capabilities::by_id("matrix.rooms.list").unwrap().group.unwrap();
        permissions.set(&subject, denied_group, GrantState::Denied);
        apply_directory_capability_defaults(&mut permissions, &room);
        assert_eq!(permissions.state(&subject, denied_group), GrantState::Denied);
        for cap_id in a2app_core::task_grants::DIRECTORY_CAP_IDS {
            let cap = a2app_core::capabilities::by_id(cap_id).unwrap();
            let group = cap.group.unwrap();
            let effective = permissions.effective_capability_for_in_context(
                &subject, ai_room_declares_perm, ai_room_declares_cap, cap,
                PermissionContext { origin_room: Some(SOURCE), target_room: Some(SOURCE) },
            );
            if group == denied_group {
                assert_ne!(effective, Effective::Granted, "{cap_id} must stay denied");
            } else {
                assert_eq!(effective, Effective::Granted, "{cap_id}");
                assert_eq!(permissions.state(&subject, group), GrantState::Ask,
                    "the group itself is not flipped to Granted");
            }
        }
        // The room's own reads and the app list are not defaulted.
        for group in [Permission::MatrixRoomRead, Permission::MatrixRoomInfo, Permission::AppLaunch] {
            assert_eq!(permissions.state(&subject, group), GrantState::Ask, "{group:?}");
        }
        // The persisted per-room marker is what makes a later Ask stick: the
        // wrapper returns before re-applying when the room is already marked.
        let mut applied = BTreeSet::new();
        assert!(applied.insert(SOURCE.to_string()), "first session applies the defaults");
        let a_group = a2app_core::capabilities::by_id("matrix.spaces.list").unwrap().group.unwrap();
        permissions.set(&subject, a_group, GrantState::Ask);
        assert!(!applied.insert(SOURCE.to_string()), "a later session is already marked");
        assert_eq!(permissions.state(&subject, a_group), GrantState::Ask, "the user's Ask survives a restart");
    }

    #[cfg(unix)]
    #[test]
    fn directory_defaults_preserve_saved_ask_and_narrow_or_withdrawn_choices() {
        let room: OwnedRoomId = SOURCE.try_into().unwrap();
        let subject = agent_subject(SOURCE);
        for cap_id in a2app_core::task_grants::DIRECTORY_CAP_IDS {
            let cap = a2app_core::capabilities::by_id(cap_id).unwrap();
            let group = cap.group.unwrap();
            for choice in 0..6 {
                let mut permissions = PermissionStore::default();
                match choice {
                    0 => permissions.set(&subject, group, GrantState::Ask),
                    1 => permissions.ask_again(&subject, group),
                    4 => permissions.grant_until(&subject, group, 0),
                    5 => permissions.grant_once(&subject, group),
                    _ => {
                        let grant = permissions.grant_scoped(&subject, group, Some(cap.id),
                            RoomScope::room(TARGET), GrantDuration::Always, None).unwrap();
                        if choice == 3 { permissions.remove_scoped_grant(grant); }
                    }
                }
                apply_directory_capability_defaults(&mut permissions, &room);
                assert!(!permissions.scoped_grants(&subject).iter().any(|grant|
                    grant.capability.as_deref() == Some(cap.id) && grant.scope == RoomScope::AllRooms),
                    "{cap_id}: defaults must not broaden saved choice {choice}");
                if choice == 5 { permissions.clear_once_for(&subject); }
                assert_ne!(permissions.effective_capability_for_in_context(
                    &subject, ai_room_declares_perm, ai_room_declares_cap, cap,
                    PermissionContext { origin_room: Some(SOURCE), target_room: Some(SPACE) },
                ), Effective::Granted, "{cap_id}: saved choice {choice} must still gate other rooms");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn space_directory_dispatch_keeps_only_directory_provenance_and_query_authorization() {
        use a2app_core::information_flow::{self as flow, Source};
        let account = format!("@space-directory-dispatch-{}:test", std::process::id());
        let previous_account = super::super::information_flow::TEST_ACCOUNT.with(|a| a.replace(Some(account)));
        let previous_state = A2APP.with(|state| state.replace(None));
        initialize_state(AppRegistry::new(Vec::new()), PermissionStore::default(), A2AppPersistedState::default(), Default::default());
        let room: OwnedRoomId = SOURCE.try_into().unwrap();
        let context = super::super::information_flow::begin_agent_session(SOURCE).unwrap();
        let provider = flow::Recipient::ModelProvider(format!("space-directory-provider-{}", std::process::id()));
        super::super::information_flow::ensure_agent_default_sharing(
            &context, SOURCE, Some(match &provider { flow::Recipient::ModelProvider(id) => id, _ => unreachable!() }),
            None, &mut BTreeSet::new(),
        );
        with_a2app(|state| {
            state.ai_rooms.insert(room.clone(), AiRoomInfo::new(None));
            apply_directory_capability_defaults(&mut state.permissions, &room);
        });
        let mut cx = Cx::new(Box::new(|_, _| {}));
        for kind in [ReadToolKind::SpaceInfo { space: SPACE.into() }, ReadToolKind::SpaceRooms { space: SPACE.into() }] {
            let (answer, _reply) = std::sync::mpsc::channel();
            execute_session_job(&mut cx, &WidgetRef::empty(), &room, SessionJob::ReadTool { kind: kind.clone(), answer });
            assert!(with_a2app(|state| state.ai_reads.values().any(|(_, dispatched, _)|
                read_tool_name(dispatched) == read_tool_name(&kind))).unwrap(), "the directory read was dispatched");
            let labels = flow::labels(&context).unwrap();
            assert!(labels.contains(&super::super::information_flow::directory_source(&context)));
            assert!(!labels.contains(&Source::Room { account: context.account().into(), room: SPACE.into() }));
            assert!(flow::ensure_allowed(&context, &provider).is_ok(), "directory tools must not block the next model call");
            let auth = with_a2app(|state| ai_read_authorization(&room, &kind, &context, &state.permissions)).flatten().unwrap();
            assert!(with_a2app(|state| auth.permits_request(&state.permissions)).unwrap());
            if matches!(kind, ReadToolKind::SpaceRooms { .. }) {
                assert_eq!(auth.flow_payload, Some(serde_json::json!({ "space_id": SPACE })));
            }
        }
        let _ = flow::remove_context(&context);
        A2APP.with(|state| { state.replace(previous_state); });
        super::super::information_flow::TEST_ACCOUNT.with(|a| { a.replace(previous_account); });
    }

    #[cfg(unix)]
    #[test]
    fn directory_result_delivery_obeys_selected_scope_and_current_denials() {
        use a2app_core::information_flow as flow;
        let account = format!("@directory-delivery-{}:test", std::process::id());
        let previous_account = super::super::information_flow::TEST_ACCOUNT.with(|a| a.replace(Some(account)));
        let previous_state = A2APP.with(|state| state.replace(None));
        initialize_state(AppRegistry::new(Vec::new()), PermissionStore::default(), A2AppPersistedState::default(), Default::default());
        let room: OwnedRoomId = SOURCE.try_into().unwrap();
        let context = super::super::information_flow::begin_agent_session(SOURCE).unwrap();
        let cap = a2app_core::capabilities::by_id("matrix.rooms.list").unwrap();
        let subject = agent_subject(SOURCE);
        with_a2app(|state| {
            state.permissions.grant_scoped(&subject, cap.group.unwrap(), Some(cap.id),
                RoomScope::room(TARGET), GrantDuration::Always, None).unwrap();
        });
        let auth = with_a2app(|state| ai_read_authorization(&room, &ReadToolKind::ListRooms, &context, &state.permissions)).flatten().unwrap();
        let response = serde_json::json!({ "rooms": [
            { "room_id": TARGET, "name": "Allowed" }, { "room_id": SPACE, "name": "Private" },
        ] }).to_string();
        let mut cx = Cx::new(Box::new(|_, _| {}));
        for denial in 0..3 {
            let (answer, reply) = std::sync::mpsc::channel();
            with_a2app(|state| {
                if denial == 1 { state.permissions.set_capability(&subject, cap.id, GrantState::Denied); }
                if denial == 2 {
                    state.permissions.set_capability(&subject, cap.id, GrantState::Ask);
                    state.permissions.set(&subject, cap.group.unwrap(), GrantState::Denied);
                }
                state.ai_reads.insert(99, (room.clone(), ReadToolKind::ListRooms, answer));
            });
            apply_ai_room_action(&mut cx, &WidgetRef::empty(), AiRoomAction::ToolReadResult {
                id: 99, result: Ok(response.clone()), authorization: Some(auth.clone()),
            });
            let result = reply.try_recv().unwrap();
            if denial == 0 {
                let value: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
                assert_eq!(value["rooms"], serde_json::json!([{ "room_id": TARGET, "name": "Allowed" }]));
            } else { assert!(result.is_err(), "a current directory denial must win over captured consent"); }
        }
        let _ = flow::remove_context(&context);
        A2APP.with(|state| { state.replace(previous_state); });
        super::super::information_flow::TEST_ACCOUNT.with(|a| { a.replace(previous_account); });
    }

    #[cfg(unix)]
    #[test]
    fn session_jobs_expose_the_worker_exact_action_and_payload() {
        let room: OwnedRoomId = SOURCE.try_into().unwrap();
        let (answer, _) = std::sync::mpsc::channel();
        let post = SessionJob::PostRoomMessage { room_id: TARGET.into(), text: "hello".into(), answer };
        let (action, payload) = session_job_exact_action(&room, &post).unwrap();
        assert_eq!(action.kind, "matrix.rooms.message.send");
        assert_eq!(action.target, TARGET);
        assert_eq!(payload, serde_json::json!({ "room_id": TARGET, "text": "hello" }));

        let (answer, _) = std::sync::mpsc::channel();
        let generate = SessionJob::LaunchSplashApp { description: "an app".into(), answer };
        let (action, payload) = session_job_exact_action(&room, &generate).unwrap();
        assert_eq!(action.kind, "apps.generate");
        assert_eq!(action.target, SOURCE);
        assert_eq!(payload, serde_json::json!({ "description": "an app", "room_id": SOURCE }));

        let (answer, _) = std::sync::mpsc::channel();
        assert!(session_job_exact_action(&room, &SessionJob::SendRoomMessage { text: "x".into(), answer }).is_none(),
            "the own-room reply is exempt and has no exact action");
    }

    #[cfg(unix)]
    #[test]
    fn exact_review_info_shows_the_exact_target_and_payload() {
        let action = a2app_core::information_flow::SensitiveAction { kind: "matrix.rooms.message.send".into(), target: TARGET.into() };
        let info = exact_review_info(&action, &serde_json::json!({ "room_id": TARGET, "text": "hello" }));
        assert_eq!(info.items.len(), 1);
        assert!(info.items[0].detail.contains(TARGET));
        assert!(info.items[0].detail.contains("hello"));
        assert!(info.items[0].grantable && info.items[0].checked, "the exact item starts checked");
    }

    #[cfg(unix)]
    struct ModalTestGuard {
        previous_state: Option<A2AppState>,
        previous_account: Option<String>,
        contexts: Vec<a2app_core::information_flow::ContextId>,
    }

    #[cfg(unix)]
    impl ModalTestGuard {
        fn new(name: &str) -> Self {
            let previous_state = A2APP.with(|state| state.replace(None));
            let account = format!("modal-{name}-{}", std::process::id());
            let previous_account = super::super::information_flow::TEST_ACCOUNT.with(|a| a.replace(Some(account)));
            let mut guard = Self { previous_state, previous_account, contexts: Vec::new() };
            initialize_state(AppRegistry::new(Vec::new()), PermissionStore::default(), A2AppPersistedState::default(), Default::default());
            for room in [SOURCE, TARGET] {
                guard.contexts.push(super::super::information_flow::prepare_agent(room).unwrap());
            }
            guard
        }
    }

    #[cfg(unix)]
    impl Drop for ModalTestGuard {
        fn drop(&mut self) {
            for context in &self.contexts { let _ = a2app_core::information_flow::remove_context(context); }
            A2APP.with(|state| { state.replace(self.previous_state.take()); });
            super::super::information_flow::TEST_ACCOUNT.with(|a| { a.replace(self.previous_account.take()); });
        }
    }

    #[cfg(unix)]
    fn modal_task_fixture(task_id: u64, answer: Sender<Result<String, String>>) -> TaskPrompt {
        use a2app_core::task_grants::{ItemOrigin, PlanAction, PlanItem};
        let context = super::super::information_flow::agent_context(SOURCE).unwrap();
        let epoch = a2app_core::information_flow::context_epoch(&context).unwrap();
        let mut plan = TaskPlan {
            task_id,
            subject: agent_subject(SOURCE),
            context, epoch,
            title: "Read a status page".into(),
            explanation: "I want to read the status page.".into(),
            items: vec![PlanItem {
                id: "website".into(), origin: ItemOrigin::Requested,
                action: PlanAction::Network { url: "https://status.example.org/".into(), scope: RoomScope::room(SOURCE) },
                state: ItemState::NeedsGrant, why: None, risk: a2app_core::capabilities::Risk::High,
            }],
            flow_dependencies: BTreeMap::new(), plan_hash: [0; 32], needs_fingerprint: [0; 32],
        };
        plan.seal();
        TaskPrompt { room_id: SOURCE.try_into().unwrap(), plan, resume: TaskResume::AgentTool(answer) }
    }

    #[cfg(unix)]
    fn modal_exact_fixture(request_id: u64, answer: Sender<Result<String, String>>) -> ExactReviewPrompt {
        let action = a2app_core::information_flow::SensitiveAction { kind: "matrix.rooms.message.send".into(), target: TARGET.into() };
        let payload = serde_json::json!({ "room_id": TARGET, "text": "A reviewed message" });
        let context = super::super::information_flow::agent_context(TARGET).unwrap();
        let epoch = a2app_core::information_flow::context_epoch(&context).unwrap();
        ExactReviewPrompt {
            room_id: TARGET.try_into().unwrap(),
            context, epoch, info: exact_review_info(&action, &payload), action, payload,
            expected: Default::default(), request_id,
            resume: ExactResume::Job(SessionJob::PostRoomMessage { room_id: TARGET.into(), text: "A reviewed message".into(), answer }),
        }
    }

    #[cfg(unix)]
    fn modal_ordinary_fixture() -> PermissionPrompt {
        PermissionPrompt {
            id: next_permission_prompt_id(), setup: None, subject: agent_subject(SOURCE),
            perm: Permission::MatrixRoomRead, parked: Vec::new(), tool: None,
            flow: None, enable_writes: false, activations: Vec::new(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn active_task_keeps_its_caller_and_dispatches_a_queued_exact_review_after_answer() {
        let _restore = ModalTestGuard::new("task-first");
        let (task_answer, task_reply) = std::sync::mpsc::channel();
        let (exact_answer, exact_reply) = std::sync::mpsc::channel();
        let review_id = u64::MAX - 1;
        with_a2app(|state| {
            state.active_task = Some(modal_task_fixture(101, task_answer));
            state.exact_reviews.push_back(modal_exact_fixture(review_id, exact_answer));
            state.prompts.push_back(modal_ordinary_fixture());
        });
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let ui = WidgetRef::empty();
        show_next_exact_review(&mut cx, &ui);
        permission_batch::show_next_ordinary(&mut cx, &ui);
        permission_batch::show_next(&mut cx, &ui);
        with_a2app(|state| {
            assert_eq!(state.active_task.as_ref().unwrap().plan.task_id, 101);
            assert!(state.active_exact_review.is_none() && state.active_prompt.is_none());
            assert_eq!(state.exact_reviews.len(), 1);
            assert_eq!(state.prompts.len(), 1);
        });
        assert!(matches!(task_reply.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));
        assert!(matches!(exact_reply.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));

        answer_task_prompt(&mut cx, &ui, TaskPermissionAction::NotNow);
        let outcome: serde_json::Value = serde_json::from_str(&task_reply.try_recv().unwrap().unwrap()).unwrap();
        assert_eq!(outcome["task_id"], 101);
        assert_eq!(outcome["status"], "declined", "the answer must reach the displayed task's caller");
        with_a2app(|state| {
            assert!(state.active_task.is_none() && state.active_prompt.is_none());
            assert_eq!(state.active_exact_review.as_ref().unwrap().request_id, review_id);
            assert!(state.exact_reviews.is_empty());
            assert_eq!(state.prompts.len(), 1, "ordinary prompts wait behind the exact review");
        });
        assert!(matches!(exact_reply.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));
    }

    #[cfg(unix)]
    #[test]
    fn active_exact_review_keeps_task_and_ordinary_prompts_queued_until_answer() {
        let _restore = ModalTestGuard::new("exact-first");
        let (task_answer, task_reply) = std::sync::mpsc::channel();
        let (exact_answer, exact_reply) = std::sync::mpsc::channel();
        let review_id = u64::MAX - 2;
        with_a2app(|state| {
            state.active_exact_review = Some(modal_exact_fixture(review_id, exact_answer));
            state.task_prompts.push_back(modal_task_fixture(102, task_answer));
            state.prompts.push_back(modal_ordinary_fixture());
        });
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let ui = WidgetRef::empty();
        show_next_task_prompt(&mut cx, &ui);
        permission_batch::show_next_ordinary(&mut cx, &ui);
        permission_batch::show_next(&mut cx, &ui);
        with_a2app(|state| {
            assert_eq!(state.active_exact_review.as_ref().unwrap().request_id, review_id);
            assert!(state.active_task.is_none() && state.active_prompt.is_none());
            assert_eq!(state.task_prompts.len(), 1);
            assert_eq!(state.prompts.len(), 1);
        });
        assert!(matches!(task_reply.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));
        assert!(matches!(exact_reply.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));

        answer_task_prompt(&mut cx, &ui, TaskPermissionAction::NotNow);
        assert!(exact_reply.try_recv().unwrap().unwrap_err().contains("did not allow"));
        with_a2app(|state| {
            assert!(state.active_exact_review.is_none() && state.active_prompt.is_none());
            assert_eq!(state.active_task.as_ref().unwrap().plan.task_id, 102);
            assert!(state.task_prompts.is_empty());
            assert_eq!(state.prompts.len(), 1, "ordinary prompts wait behind the task");
        });
        assert!(matches!(task_reply.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));
    }

    #[cfg(unix)]
    #[test]
    fn network_prompt_retains_the_full_destination_and_source_room() {
        let (answer, _) = std::sync::mpsc::channel();
        let url = "https://example.org:8443/path?query=1";
        let parked = ParkedRequest::NetworkAccess {
            room_id: SOURCE.try_into().unwrap(), host: "example.org".into(), url: url.into(), answer,
        };
        assert_eq!(parked_network_url(&parked).as_deref(), Some(url));
        assert_eq!(parked_rooms(&parked), (Some(SOURCE.into()), Some(SOURCE.into())));
        assert_eq!(parked_capability(&parked).unwrap().id, "network.http");
    }

    #[cfg(unix)]
    #[test]
    fn directory_reads_are_blocked_by_the_group_and_filtered_by_room_deny() {
        let subject = agent_subject(SOURCE);
        let cap = a2app_core::capabilities::by_id("matrix.rooms.list").unwrap();
        let group = cap.group.unwrap();
        let context = PermissionContext { origin_room: Some(SOURCE), target_room: Some(SOURCE) };
        let verdict = |store: &PermissionStore| store.effective_collection_capability_for_in_context(
            &subject, |_| true, |_| true, cap, context,
        );
        let mut permissions = PermissionStore::default();
        assert_eq!(verdict(&permissions), Effective::NeedsPrompt);
        // A Deny on the directory group is the kill switch for the whole call.
        permissions.set(&subject, group, GrantState::Denied);
        assert_eq!(verdict(&permissions), Effective::Denied);
        // A room whose read policy is Deny never appears in a directory result.
        permissions.set_room_policy(TARGET, RoomAccess::Read, PolicyDecision::Deny);
        let filtered = matrix::policy::filter_read_result(
            &serde_json::json!({ "rooms": [
                { "room_id": SOURCE, "name": "Kept" },
                { "room_id": TARGET, "name": "Hidden" },
            ] }).to_string(),
            &permissions, None,
        ).unwrap();
        let value: serde_json::Value = serde_json::from_str(&filtered).unwrap();
        let rooms = value["rooms"].as_array().unwrap();
        assert_eq!(rooms.len(), 1);
        assert_eq!(rooms[0]["room_id"], SOURCE);
    }

    #[cfg(unix)]
    #[test]
    fn permission_store_guard_restores_the_store_on_drop() {
        initialize_state(AppRegistry::new(Vec::new()), PermissionStore::default(), A2AppPersistedState::default(), Default::default());
        with_a2app(|state| state.permissions.set("guard-test", Permission::Network, GrantState::Granted)).unwrap();
        let mut guard = PermissionStoreGuard::take();
        // A different entry, as an apply would add; the original must survive.
        guard.store().set("guard-test", Permission::Camera, GrantState::Denied);
        drop(guard);
        let restored = with_a2app(|state| state.permissions.state("guard-test", Permission::Network)).unwrap();
        assert_eq!(restored, GrantState::Granted, "an early return or panic must not lose the store");
    }

    #[cfg(unix)]
    #[test]
    fn scoped_item_detail_lists_target_names_without_a_repeated_noun() {
        let action = a2app_core::task_grants::PlanAction::Scoped {
            permission: "matrix-rooms-read".into(),
            capability: "matrix.rooms.messages.read".into(),
            scope: RoomScope::Selection { rooms: vec![TARGET.into()], spaces: Vec::new() },
        };
        let detail = task_item_detail(&action, &|id: &str| id.to_string(), None, &|_tool: &str| None::<String>);
        assert_eq!(detail, TARGET);
        assert!(!detail.contains("rooms:"), "the title already names the kind");
    }

    #[cfg(unix)]
    #[test]
    fn task_request_cap_is_per_room_and_dismissal_covers_subsets() {
        let mut counts = HashMap::new();
        for _ in 0..3 {
            assert!(count_task_request(&mut counts, SOURCE), "three requests are allowed");
        }
        assert!(!count_task_request(&mut counts, SOURCE), "the fourth request in a turn is refused");
        assert!(count_task_request(&mut counts, TARGET), "another room keeps its own budget");

        let original: BTreeSet<String> = ["needs:a", "needs:b"].into_iter().map(String::from).collect();
        let mut dismissed = HashMap::new();
        dismissed.insert((SOURCE.to_string(), [7; 32]), original.clone());
        // The same needs renamed (different fingerprint) are still covered.
        assert!(task_plan_is_dismissed(&dismissed, SOURCE, &original));
        assert!(task_plan_is_dismissed(&dismissed, SOURCE, &["needs:a".to_string()].into_iter().collect()));
        // One new need makes it a strict superset, which is a new request.
        let with_new: BTreeSet<String> = ["needs:a", "needs:b", "needs:c"].into_iter().map(String::from).collect();
        assert!(!task_plan_is_dismissed(&dismissed, SOURCE, &with_new));
        assert!(!task_plan_is_dismissed(&dismissed, TARGET, &["needs:a".to_string()].into_iter().collect()));
    }

    #[cfg(unix)]
    #[test]
    fn combining_separately_declined_needs_does_not_reopen_the_prompt() {
        let mut dismissed = HashMap::new();
        dismissed.insert((SOURCE.to_string(), [1; 32]), BTreeSet::from(["needs:a".into()]));
        dismissed.insert((SOURCE.to_string(), [2; 32]), BTreeSet::from(["needs:b".into()]));
        dismissed.insert((TARGET.to_string(), [3; 32]), BTreeSet::from(["needs:c".into()]));
        let combined = BTreeSet::from(["needs:a".into(), "needs:b".into()]);
        assert!(task_plan_is_dismissed(&dismissed, SOURCE, &combined));
        assert!(!task_plan_is_dismissed(&dismissed, TARGET, &combined));
        let with_new = BTreeSet::from(["needs:b".into(), "needs:c".into()]);
        assert!(!task_plan_is_dismissed(&dismissed, SOURCE, &with_new), "another room's decline does not cover a new need");
        assert!(!task_plan_is_dismissed(&HashMap::new(), SOURCE, &BTreeSet::new()));
    }

    #[cfg(unix)]
    #[test]
    fn the_turn_anchor_skips_the_adaptive_backoff_but_keeps_the_hard_floor() {
        let backoff = Duration::from_secs(20);
        // No prior post: anything may go now.
        assert!(turn_post_is_cooled_down(true, None, backoff));
        // A post 10s ago is inside the adaptive backoff, but the anchor skips it.
        assert!(turn_post_is_cooled_down(true, Some(Duration::from_secs(10)), backoff));
        // Inside the hard floor: even the anchor waits, so a burst of short
        // turns cannot fire back-to-back state events.
        assert!(!turn_post_is_cooled_down(true, Some(Duration::from_secs(1)), backoff));
        // A non-anchor snapshot respects the adaptive backoff.
        assert!(!turn_post_is_cooled_down(false, Some(Duration::from_secs(10)), backoff));
        assert!(turn_post_is_cooled_down(false, Some(Duration::from_secs(25)), backoff));
    }

    #[cfg(unix)]
    #[test]
    fn a_not_now_answer_dismisses_the_plans_needs_and_approves_nothing() {
        let room: OwnedRoomId = SOURCE.try_into().unwrap();
        let plan = a2app_core::task_grants::TaskPlan {
            task_id: 1,
            subject: "agent".into(),
            context: a2app_core::information_flow::ContextId::Agent { account: "alice".into(), room: SOURCE.into() },
            epoch: 1,
            title: "t".into(),
            explanation: "e".into(),
            items: vec![a2app_core::task_grants::PlanItem {
                id: "n1".into(), origin: a2app_core::task_grants::ItemOrigin::Requested,
                action: a2app_core::task_grants::PlanAction::Network {
                    url: "https://example.org/".into(), scope: RoomScope::room(SOURCE),
                },
                state: ItemState::NeedsGrant, why: None, risk: a2app_core::capabilities::Risk::High,
            }],
            flow_dependencies: BTreeMap::new(),
            plan_hash: [0; 32],
            needs_fingerprint: [9; 32],
        };
        let mut dismissed = HashMap::new();
        let approved = task_answer_approval(&room, &plan, &TaskPermissionAction::NotNow, &mut dismissed);
        assert!(approved.is_empty(), "Not now approves nothing");
        assert!(dismissed.contains_key(&(SOURCE.to_string(), [9; 32])), "Not now must be remembered for the turn");
        // An Allow keeps the checked ids and does not dismiss the plan.
        let dismissed_len = dismissed.len();
        let approved = task_answer_approval(
            &room, &plan, &TaskPermissionAction::Allow(vec!["n1".into()]), &mut dismissed);
        assert!(approved.contains("n1"));
        assert_eq!(dismissed.len(), dismissed_len);
    }

    #[cfg(unix)]
    #[test]
    fn revoking_a_turn_drops_its_grants_dismissals_and_request_budget() {
        let _published_grants = PublishedGrantsTestGuard::new();
        initialize_state(
            AppRegistry::new(Vec::new()),
            PermissionStore::default(),
            A2AppPersistedState::default(),
            Default::default(),
        );
        let room: OwnedRoomId = SOURCE.try_into().unwrap();
        let context = a2app_core::information_flow::ContextId::Agent { account: "alice".into(), room: SOURCE.into() };
        with_a2app(|state| {
            state.task_ledger.insert(a2app_core::task_grants::AppliedTask {
                task_id: 42,
                subject: "agent".into(),
                context,
                epoch: 1,
                title: "t".into(),
                plan_hash: [0; 32],
                grants: Vec::new(),
                item_states: BTreeMap::new(),
            });
            state.dismissed_task_plans.insert((SOURCE.to_string(), [3; 32]), BTreeSet::new());
            state.task_request_counts.insert(SOURCE.to_string(), 2);
            state.task_request_counts.insert(TARGET.to_string(), 1);
        })
        .unwrap();
        revoke_task_grants(&room);
        assert!(with_a2app(|state| state.task_ledger.get(42).is_none()).unwrap(), "the turn's grants are gone");
        assert!(with_a2app(|state| state.dismissed_task_plans.keys().all(|(dismissed_room, _)| dismissed_room != SOURCE)).unwrap());
        assert!(with_a2app(|state| !state.task_request_counts.contains_key(SOURCE)).unwrap());
        assert!(
            with_a2app(|state| state.task_request_counts.contains_key(TARGET)).unwrap(),
            "another room keeps its own request budget"
        );
    }

    /// A plan that needs no prompt still gets an honest tool receipt: success
    /// only when everything is already allowed, a failed "Blocked by room
    /// policy" when everything is blocked, and a failed mixed note otherwise.
    #[cfg(unix)]
    #[test]
    fn a_settled_plan_receipt_distinguishes_allowed_from_blocked() {
        use a2app_core::task_grants::{ItemOrigin, PlanAction, PlanItem};
        let plan = |states: Vec<ItemState>| {
            let items = states.into_iter().enumerate().map(|(index, state)| PlanItem {
                id: format!("n{index}"),
                origin: ItemOrigin::Requested,
                action: PlanAction::Network { url: "https://example.com/".into(), scope: RoomScope::room(SOURCE) },
                state,
                why: None,
                risk: a2app_core::capabilities::Risk::Low,
            }).collect();
            TaskPlan {
                task_id: 1, subject: "s".into(),
                context: a2app_core::information_flow::ContextId::Agent { account: "a".into(), room: SOURCE.into() },
                epoch: 1, title: "t".into(), explanation: "e".into(),
                plan_hash: [0; 32], needs_fingerprint: [0; 32], items, flow_dependencies: BTreeMap::new(),
            }
        };
        assert_eq!(settled_task_receipt(&plan(vec![ItemState::AlreadyAllowed])), (true, "Nothing new to allow"));
        assert_eq!(settled_task_receipt(&plan(vec![ItemState::Blocked(TaskReason::BlockedByRoomPolicy)])),
            (false, "Blocked by room policy"));
        assert_eq!(settled_task_receipt(&plan(vec![ItemState::Blocked(TaskReason::BlockedByPermission)])),
            (false, "Blocked by permission settings"));
        assert_eq!(settled_task_receipt(&plan(vec![ItemState::NotOffered(TaskReason::NotOffered)])),
            (false, "Not offered"));
        assert_eq!(settled_task_receipt(&plan(vec![ItemState::AlreadyAllowed, ItemState::NotOffered(TaskReason::NotOffered)])),
            (false, "Some needs were refused"));
    }

    /// The directory defaults must grant the exact directory capabilities, not
    /// their whole permission groups: room search, previews, invites and the
    /// incoming room-list hooks share `MatrixRoomsList`, and `MatrixSpaces` is
    /// shared with the space-changed hook. The store's group state stays Ask;
    /// only a scoped grant for each directory capability is recorded.
    #[cfg(unix)]
    #[test]
    fn directory_defaults_grant_a_shared_group_only_by_capability() {
        use a2app_core::capabilities::CATALOG;
        use a2app_core::task_grants::DIRECTORY_CAP_IDS;
        let room: OwnedRoomId = SOURCE.try_into().unwrap();
        let mut permissions = PermissionStore::default();
        assert!(apply_directory_capability_defaults(&mut permissions, &room));
        let subject = agent_subject(SOURCE);
        let directory_groups: BTreeSet<Permission> = DIRECTORY_CAP_IDS.iter()
            .filter_map(|id| a2app_core::capabilities::by_id(id).and_then(|cap| cap.group))
            .collect();
        // A caller that declares the whole group: with a group-level default
        // every sibling would be granted, which is exactly the bug.
        let declares_perm = |_permission: Permission| true;
        let declares_cap = |_cap: &a2app_core::capabilities::Capability| true;
        let mut saw_shared_sibling = false;
        for cap in CATALOG.iter() {
            let Some(group) = cap.group else { continue };
            if !directory_groups.contains(&group) {
                continue;
            }
            let effective = permissions.effective_capability_for_in_context(
                &subject, declares_perm, declares_cap, cap,
                PermissionContext { origin_room: Some(SOURCE), target_room: Some(SOURCE) },
            );
            if DIRECTORY_CAP_IDS.contains(&cap.id) {
                assert_eq!(effective, Effective::Granted, "{} must be granted", cap.id);
            } else {
                saw_shared_sibling = true;
                assert_ne!(effective, Effective::Granted,
                    "{} shares a directory permission group but must not be granted by the defaults", cap.id);
            }
        }
        assert!(saw_shared_sibling, "the catalog must have a non-directory capability sharing a directory group");
        // Re-running is idempotent.
        assert!(!apply_directory_capability_defaults(&mut permissions, &room));
    }

    /// The carried-over path follows the same apply/ledger/rollback machinery
    /// as an agent's own task: applying the host-raised plan clears the
    /// provider rows, and the turn-close rollback restores them so the next
    /// turn prompts again.
    #[cfg(unix)]
    #[test]
    fn a_carried_over_room_is_cleared_by_the_task_apply_and_restored_by_rollback() {
        let account = format!("carried-{}", std::process::id());
        let previous_account = super::super::information_flow::TEST_ACCOUNT.with(|a| a.replace(Some(account.clone())));
        let previous_state = A2APP.with(|state| state.replace(None));
        let context = super::super::information_flow::prepare_agent(SOURCE).unwrap();
        // The agent read TARGET in an earlier turn; its turn-scoped provider
        // rule is gone, but the label persists into this turn.
        let provider = Recipient::ModelProvider("carried-provider".into());
        let carried_room = Source::Room { account: account.clone(), room: TARGET.into() };
        a2app_core::information_flow::add_sources(&context, [carried_room.clone()]).unwrap();
        let carried = a2app_core::information_flow::carried_over_sources(&context, &provider).unwrap();
        assert!(carried.contains(&carried_room));
        let room_id: OwnedRoomId = SOURCE.try_into().unwrap();
        let plan = build_carried_task_plan(&room_id, &context, &carried, &provider, None, 1);
        assert!(plan.explanation.contains("Earlier in this conversation"), "the host writes the explanation");
        assert!(plan.items.iter().any(|item| item.state.is_grantable()), "the carried source needs a row");
        let approved: BTreeSet<String> = plan.items.iter().filter(|item| item.state.is_grantable())
            .map(|item| item.id.clone()).collect();
        let mut store = PermissionStore::default();
        let applied = task_grants::apply(&plan, &approved, &mut store, &task_grants::GlobalFlowApply).unwrap();
        assert!(!applied.grants.is_empty(), "the carried plan must store rules");
        assert!(a2app_core::information_flow::carried_over_sources(&context, &provider).unwrap().is_empty(),
            "applying the carried plan clears the provider rows");
        task_grants::rollback(&applied, &mut store, &task_grants::GlobalFlowApply);
        assert!(a2app_core::information_flow::carried_over_sources(&context, &provider).unwrap().contains(&carried_room),
            "the turn-close rollback restores the carried source for the next turn");
        let _ = a2app_core::information_flow::remove_context(&context);
        A2APP.with(|state| { state.replace(previous_state); });
        super::super::information_flow::TEST_ACCOUNT.with(|a| { a.replace(previous_account); });
    }

    #[cfg(unix)]
    #[test]
    fn mini_app_tool_names_resolve_only_unambiguous_tools_in_the_requested_room() {
        let previous_state = A2APP.with(|state| state.replace(None));
        initialize_state(AppRegistry::new(Vec::new()), PermissionStore::default(), A2AppPersistedState::default(), Default::default());
        let room: OwnedRoomId = SOURCE.try_into().unwrap();
        let other: OwnedRoomId = TARGET.try_into().unwrap();
        with_a2app(|state| {
            let registration = |full: &str, target: &OwnedRoomId| AppToolRegistration {
                full_name: full.into(), raw_name: "play".into(), room_id: target.clone(),
                app_id: "game".into(), heap_key: 1, description: String::new(),
                schema: serde_json::json!({}), args: Vec::new(),
            };
            state.app_tools.insert("app_game_play".into(), registration("app_game_play", &room));
            state.app_tools.insert("app_other_play".into(), registration("app_other_play", &other));
            assert_eq!(canonical_app_tool_name(state, &room, "play").as_deref(), Some("app_game_play"));
            assert_eq!(canonical_app_tool_name(state, &room, "app_game_play").as_deref(), Some("app_game_play"));
            assert_eq!(canonical_app_tool_name(state, &room, "app_other_play"), None);
            assert_eq!(canonical_app_tool_name(state, &room, "missing"), None);
            state.app_tools.insert("app_second_play".into(), registration("app_second_play", &room));
            assert_eq!(canonical_app_tool_name(state, &room, "play"), None);
            assert_eq!(canonical_app_tool_name(state, &room, "app_game_play").as_deref(), Some("app_game_play"));
        });
        A2APP.with(|state| { state.replace(previous_state); });
    }

    #[cfg(unix)]
    struct PublishedGrantsTestGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    #[cfg(unix)]
    impl PublishedGrantsTestGuard {
        fn new() -> Self {
            Self { _lock: super::super::effect_review::TEST_LOCK.lock().unwrap() }
        }
    }

    #[cfg(unix)]
    impl Drop for PublishedGrantsTestGuard {
        fn drop(&mut self) {
            let permissions = PermissionStore::default();
            a2app_core::permissions::publish_snapshot(permissions.snapshot(&AppRegistry::new(Vec::new())));
            matrix::publish_permission_policy(&permissions);
            matrix::policy::publish_effect_space_roots(BTreeSet::new());
        }
    }

    #[cfg(unix)]
    struct FinalWriteTestState {
        room: OwnedRoomId,
        context: a2app_core::information_flow::ContextId,
        previous_state: Option<A2AppState>,
        previous_account: Option<String>,
        _published_grants: PublishedGrantsTestGuard,
    }

    #[cfg(unix)]
    impl FinalWriteTestState {
        fn new(name: &str) -> Self {
            let published_grants = PublishedGrantsTestGuard::new();
            let account = format!("{name}-{}", std::process::id());
            let previous_account = super::super::information_flow::TEST_ACCOUNT.with(|a| a.replace(Some(account)));
            let previous_state = A2APP.with(|state| state.replace(None));
            let mut permissions = PermissionStore::default();
            permissions.set_matrix_write(true);
            initialize_state(AppRegistry::new(Vec::new()), permissions, A2AppPersistedState::default(), Default::default());
            let room: OwnedRoomId = SOURCE.try_into().unwrap();
            let context = super::super::information_flow::prepare_agent(SOURCE).unwrap();
            with_a2app(|state| {
                state.ai_rooms.insert(room.clone(), AiRoomInfo::new(None));
                state.task_ledger.insert(a2app_core::task_grants::AppliedTask {
                    task_id: 1, subject: agent_subject(SOURCE), context: context.clone(), epoch: 1,
                    title: "task".into(), plan_hash: [0; 32], grants: Vec::new(), item_states: BTreeMap::new(),
                });
            });
            Self { room, context, previous_state, previous_account, _published_grants: published_grants }
        }

        fn close(&self, key: &str) {
            with_a2app(|state| state.ai_rooms.get_mut(&self.room).unwrap().active_turn = Some(ActiveTurn {
                key: key.into(), tool_calls: Vec::new(), thinking: false, seq: 0, created_at: 0,
            }));
            close_active_turn(&self.room, false);
        }
    }

    #[cfg(unix)]
    impl Drop for FinalWriteTestState {
        fn drop(&mut self) {
            let _ = a2app_core::information_flow::remove_context(&self.context);
            A2APP.with(|state| { state.replace(self.previous_state.take()); });
            super::super::information_flow::TEST_ACCOUNT.with(|a| { a.replace(self.previous_account.take()); });
        }
    }

    #[cfg(unix)]
    #[test]
    fn turn_rollback_immediately_revokes_worker_permission_without_restarting_context() {
        let fixture = FinalWriteTestState::new("task-worker-rollback");
        let subject = agent_subject(SOURCE);
        let capability = a2app_core::capabilities::by_id("matrix.rooms.messages.read").unwrap();
        let epoch = a2app_core::information_flow::context_epoch(&fixture.context).unwrap();
        let authorization = with_a2app(|state| {
            let grant = state.permissions.grant_scoped(&subject, capability.group.unwrap(), Some(capability.id),
                RoomScope::room(TARGET), GrantDuration::RobrixSession, Some(SOURCE)).unwrap();
            state.task_ledger.insert(task_grants::AppliedTask {
                task_id: 2, subject: subject.clone(), context: fixture.context.clone(), epoch,
                title: "Read another room".into(), plan_hash: [0; 32],
                grants: vec![task_grants::GrantRef::Scoped(grant)], item_states: BTreeMap::new(),
            });
            let mut authorization = matrix::MatrixAuthorization::new(&subject, capability.id, Some(SOURCE), &state.permissions)
                .with_flow(fixture.context.clone());
            authorization.target_room = Some(TARGET.into());
            authorization
        }).unwrap();
        publish_current_grants();
        assert!(authorization.check_current_permission().is_ok(), "the worker sees the applied task grant");

        revoke_task_grants(&fixture.room);

        assert!(authorization.check_context().is_ok(), "the still-live agent retains its activation");
        assert_eq!(a2app_core::information_flow::context_epoch(&fixture.context).unwrap(), epoch);
        assert!(authorization.check_current_permission().is_err(), "the captured worker loses the revoked consent immediately");
        assert!(with_a2app(|state| state.task_ledger.is_empty()).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn natural_replies_have_distinct_tokens_and_stale_results_cannot_release_a_new_reply() {
        let fixture = FinalWriteTestState::new("natural-reply-tokens");
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let ui = WidgetRef::empty();
        close_active_turn(&fixture.room, false);
        post_ai_reply(&fixture.room, "first".into(), None);
        let first = with_a2app(|state| *state.ai_rooms[&fixture.room].pending_final_writes.first().unwrap()).unwrap();
        close_active_turn(&fixture.room, false);
        post_ai_reply(&fixture.room, "second".into(), None);
        let second = with_a2app(|state| {
            let writes = &state.ai_rooms[&fixture.room].pending_final_writes;
            assert_eq!(writes.len(), 2);
            *writes.iter().find(|&&id| id != first).unwrap()
        }).unwrap();
        for _ in 0..2 {
            apply_ai_room_action(&mut cx, &ui, AiRoomAction::PostReplyResult {
                room_id: fixture.room.clone(), answer_id: None, write_id: first, result: Ok(()),
            });
            assert_eq!(with_a2app(|state| state.ai_rooms[&fixture.room].pending_final_writes.clone()).unwrap(), BTreeSet::from([second]));
            assert!(with_a2app(|state| !state.task_ledger.is_empty()).unwrap());
        }
        apply_ai_room_action(&mut cx, &ui, AiRoomAction::PostReplyResult {
            room_id: fixture.room.clone(), answer_id: None, write_id: second, result: Ok(()),
        });
        assert!(with_a2app(|state| state.task_ledger.is_empty()).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn every_queued_done_snapshot_keeps_the_grants_until_its_own_completion() {
        let fixture = FinalWriteTestState::new("queued-done-snapshots");
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let ui = WidgetRef::empty();
        fixture.close("turn-one");
        fixture.close("turn-two");
        assert_eq!(with_a2app(|state| state.ai_rooms[&fixture.room].pending_final_turns.len()).unwrap(), 2);
        flush_pending_ai_turns(&mut cx);
        let first = with_a2app(|state| *state.ai_rooms[&fixture.room].pending_final_writes.first().unwrap()).unwrap();
        apply_ai_room_action(&mut cx, &ui, AiRoomAction::StateEventPosted {
            room_id: fixture.room.clone(), event_type: AI_TURN_EVENT_TYPE.into(), write_id: first, success: true,
        });
        assert!(with_a2app(|state| !state.task_ledger.is_empty()).unwrap(), "the queued second Done still holds the grants");
        with_a2app(|state| state.ai_rooms.get_mut(&fixture.room).unwrap().last_ai_turn_post = None);
        flush_pending_ai_turns(&mut cx);
        let second = with_a2app(|state| {
            let info = &state.ai_rooms[&fixture.room];
            assert!(info.pending_final_turns.is_empty());
            *info.pending_final_writes.first().unwrap()
        }).unwrap();
        assert_ne!(first, second);
        apply_ai_room_action(&mut cx, &ui, AiRoomAction::StateEventPosted {
            room_id: fixture.room.clone(), event_type: AI_TURN_EVENT_TYPE.into(), write_id: second, success: true,
        });
        assert!(with_a2app(|state| state.task_ledger.is_empty()).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn refusing_queued_snapshots_clears_their_markers_without_releasing_other_writes() {
        let fixture = FinalWriteTestState::new("refused-done-snapshots");
        let mut cx = Cx::new(Box::new(|_, _| {}));
        post_ai_reply(&fixture.room, "pending".into(), None);
        let reply = with_a2app(|state| *state.ai_rooms[&fixture.room].pending_final_writes.first().unwrap()).unwrap();
        fixture.close("turn-one");
        fixture.close("turn-two");
        with_a2app(|state| state.permissions.set_room_policy(SOURCE, RoomAccess::Write, PolicyDecision::Deny));
        flush_pending_ai_turns(&mut cx);
        post_ai_reply(&fixture.room, "refused".into(), None);
        with_a2app(|state| {
            let info = &state.ai_rooms[&fixture.room];
            assert!(info.pending_ai_turns.is_empty());
            assert!(info.pending_final_turns.is_empty());
            assert_eq!(info.pending_final_writes, BTreeSet::from([reply]));
            assert!(!state.task_ledger.is_empty());
        });
        final_write_landed(&fixture.room, reply);
        assert!(with_a2app(|state| state.task_ledger.is_empty()).unwrap());
    }

    /// Item 3's acceptance: a turn that read another room keeps its grants
    /// until the last final write (the `Done` snapshot, an error/stopped row,
    /// the reply) lands, so the whole-label output check those writes need
    /// still passes. The last landing write empties the room's ledger.
    #[cfg(unix)]
    #[test]
    fn the_turn_grants_survive_until_the_last_final_write_lands() {
        use a2app_core::information_flow as flow;
        let _published_grants = PublishedGrantsTestGuard::new();
        let account = format!("final-writes-{}", std::process::id());
        let previous_account = super::super::information_flow::TEST_ACCOUNT.with(|a| a.replace(Some(account.clone())));
        let previous_state = A2APP.with(|state| state.replace(None));
        let mut runtime_permissions = PermissionStore::default();
        runtime_permissions.set_matrix_write(true);
        initialize_state(AppRegistry::new(Vec::new()), runtime_permissions, A2AppPersistedState::default(), Default::default());
        let room_id: OwnedRoomId = SOURCE.try_into().unwrap();
        let context = super::super::information_flow::prepare_agent(SOURCE).unwrap();
        let provider = Recipient::ModelProvider("final-provider".into());
        let homeserver = Recipient::network_origin("https://hs.final.example.org").unwrap();
        let own_room = Recipient::MatrixRoom { account: account.clone(), room: SOURCE.into() };
        let room_b = Source::Room { account: account.clone(), room: TARGET.into() };
        // The default own-room plumbing plus the turn's cross-room read.
        super::super::information_flow::ensure_agent_default_sharing(
            &context, SOURCE, Some("final-provider"), Some("https://hs.final.example.org"),
            &mut BTreeSet::new(),
        );
        flow::add_sources(&context, [room_b.clone()]).unwrap();
        let plan = build_carried_task_plan(&room_id, &context, &[room_b.clone()].into_iter().collect(),
            &provider, Some(&homeserver), 7);
        let approved: BTreeSet<String> = plan.items.iter().map(|item| item.id.clone()).collect();
        let mut store = PermissionStore::default();
        let applied = task_grants::apply(&plan, &approved, &mut store, &task_grants::GlobalFlowApply).unwrap();
        with_a2app(|state| {
            state.ai_rooms.insert(room_id.clone(), AiRoomInfo::new(None));
            let info = state.ai_rooms.get_mut(&room_id).unwrap();
            info.active_turn = Some(ActiveTurn {
                key: "turn-1".into(), tool_calls: Vec::new(), thinking: false, seq: 0, created_at: 0,
            });
            state.task_ledger.insert(applied);
        });
        // The turn closes with a Done snapshot on its card. Closing must not
        // revoke: the caller has not queued the reply/activity yet, and the
        // Done is not even submitted until the flush runs.
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let ui = WidgetRef::empty();
        close_active_turn(&room_id, false);
        settle_closed_turn(&room_id);
        assert!(with_a2app(|state| state.task_ledger.get(7).is_some()).unwrap(),
            "closing a turn must not revoke before its Done snapshot is submitted");
        // The flush submits the Done and registers its final-write token.
        flush_pending_ai_turns(&mut cx);
        let token = with_a2app(|state| {
            let info = state.ai_rooms.get(&room_id)?;
            assert!(info.pending_final_turns.is_empty(), "the flush consumed the queued Done");
            info.pending_final_writes.iter().next().copied()
        })
        .flatten()
        .expect("the flush registers the final snapshot's token");
        // While the write is outstanding, the whole-label output check
        // `ensure_ai_state_output` makes (the room and the homeserver origin)
        // still passes, so the write is not refused.
        assert!(flow::ensure_allowed(&context, &own_room).is_ok());
        assert!(flow::ensure_allowed(&context, &homeserver).is_ok());
        assert!(with_a2app(|state| state.task_ledger.get(7).is_some()).unwrap(),
            "an outstanding final write keeps the turn's grants");
        // The Done snapshot lands: the ledger is empty and the grants are gone.
        apply_ai_room_action(&mut cx, &ui, AiRoomAction::StateEventPosted {
            room_id: room_id.clone(), event_type: AI_TURN_EVENT_TYPE.into(), write_id: token, success: true,
        });
        assert!(with_a2app(|state| state.task_ledger.is_empty()).unwrap(),
            "the last final write empties the room's ledger");
        assert!(flow::ensure_allowed(&context, &homeserver).is_err());
        assert!(flow::ensure_allowed(&context, &own_room).is_err());
        let _ = flow::remove_context(&context);
        A2APP.with(|state| { state.replace(previous_state); });
        super::super::information_flow::TEST_ACCOUNT.with(|a| { a.replace(previous_account); });
    }

    /// A turn with no tool calls (no card) has no Done snapshot. Closing it
    /// must still leave the revoke to the caller after it queues the reply, or
    /// the reply would be refused because the grants were already gone.
    #[cfg(unix)]
    #[test]
    fn closing_a_cardless_turn_keeps_grants_for_the_reply() {
        let _published_grants = PublishedGrantsTestGuard::new();
        let account = format!("cardless-{}", std::process::id());
        let previous_account = super::super::information_flow::TEST_ACCOUNT.with(|a| a.replace(Some(account.clone())));
        let previous_state = A2APP.with(|state| state.replace(None));
        initialize_state(AppRegistry::new(Vec::new()), PermissionStore::default(), A2AppPersistedState::default(), Default::default());
        let room_id: OwnedRoomId = SOURCE.try_into().unwrap();
        let context = a2app_core::information_flow::ContextId::Agent { account: account.clone(), room: SOURCE.into() };
        with_a2app(|state| {
            state.ai_rooms.insert(room_id.clone(), AiRoomInfo::new(None));
            state.task_ledger.insert(a2app_core::task_grants::AppliedTask {
                task_id: 9, subject: "agent".into(), context: context.clone(), epoch: 1,
                title: "t".into(), plan_hash: [0; 32], grants: Vec::new(), item_states: BTreeMap::new(),
            });
        });
        // No active turn: closing it must not revoke on its own. The caller
        // queues the reply, then settles.
        close_active_turn(&room_id, false);
        assert!(with_a2app(|state| state.task_ledger.get(9).is_some()).unwrap(),
            "a cardless turn must keep its grants until the reply is queued");
        // The reply final write is the only outstanding token; it keeps the
        // grants until it lands.
        let write_id = NEXT_AI_WRITE_ID.fetch_add(1, Ordering::Relaxed);
        register_final_write(&room_id, write_id);
        settle_closed_turn(&room_id);
        assert!(with_a2app(|state| state.task_ledger.get(9).is_some()).unwrap(),
            "the queued reply keeps the grants until it lands");
        final_write_landed(&room_id, write_id);
        assert!(with_a2app(|state| state.task_ledger.is_empty()).unwrap(),
            "the landed reply empties the room's ledger");
        A2APP.with(|state| { state.replace(previous_state); });
        super::super::information_flow::TEST_ACCOUNT.with(|a| { a.replace(previous_account); });
    }

    /// Two final writes outstanding at once (an error row and a stopped row in
    /// one pass) each hold a token: the grants stay live until both land, so a
    /// single boolean can never strand the second one.
    #[cfg(unix)]
    #[test]
    fn two_final_writes_each_hold_the_grants_until_they_land() {
        let _published_grants = PublishedGrantsTestGuard::new();
        let previous_state = A2APP.with(|state| state.replace(None));
        initialize_state(AppRegistry::new(Vec::new()), PermissionStore::default(), A2AppPersistedState::default(), Default::default());
        let room_id: OwnedRoomId = SOURCE.try_into().unwrap();
        let context = a2app_core::information_flow::ContextId::Agent { account: "alice".into(), room: SOURCE.into() };
        with_a2app(|state| {
            state.ai_rooms.insert(room_id.clone(), AiRoomInfo::new(None));
            state.task_ledger.insert(a2app_core::task_grants::AppliedTask {
                task_id: 11, subject: "agent".into(), context, epoch: 1,
                title: "t".into(), plan_hash: [0; 32], grants: Vec::new(), item_states: BTreeMap::new(),
            });
        });
        close_active_turn(&room_id, false);
        register_final_write(&room_id, 1);
        register_final_write(&room_id, 2);
        settle_closed_turn(&room_id);
        assert!(with_a2app(|state| state.task_ledger.get(11).is_some()).unwrap(),
            "two outstanding writes keep the grants");
        final_write_landed(&room_id, 1);
        assert!(with_a2app(|state| state.task_ledger.get(11).is_some()).unwrap(),
            "the second write still holds the grants");
        final_write_landed(&room_id, 2);
        assert!(with_a2app(|state| state.task_ledger.is_empty()).unwrap(),
            "the last write empties the ledger");
        A2APP.with(|state| { state.replace(previous_state); });
    }

    /// The failsafe: a final write that never reports back must not keep the
    /// grants alive past the deadline.
    #[cfg(unix)]
    #[test]
    fn a_stuck_final_write_is_released_by_the_failsafe_deadline() {
        let _published_grants = PublishedGrantsTestGuard::new();
        let previous_state = A2APP.with(|state| state.replace(None));
        initialize_state(AppRegistry::new(Vec::new()), PermissionStore::default(), A2AppPersistedState::default(), Default::default());
        let room_id: OwnedRoomId = SOURCE.try_into().unwrap();
        let context = a2app_core::information_flow::ContextId::Agent { account: "alice".into(), room: SOURCE.into() };
        with_a2app(|state| {
            state.ai_rooms.insert(room_id.clone(), AiRoomInfo::new(None));
            let info = state.ai_rooms.get_mut(&room_id).unwrap();
            info.final_writes_pending_revoke = true;
            info.pending_final_writes = BTreeSet::from([42u64]);
            info.final_writes_deadline = Some(Instant::now() - Duration::from_secs(1));
            state.task_ledger.insert(a2app_core::task_grants::AppliedTask {
                task_id: 12, subject: "agent".into(), context, epoch: 1,
                title: "t".into(), plan_hash: [0; 32], grants: Vec::new(), item_states: BTreeMap::new(),
            });
        });
        let mut cx = Cx::new(Box::new(|_, _| {}));
        flush_pending_ai_turns(&mut cx);
        assert!(with_a2app(|state| state.task_ledger.is_empty()).unwrap(),
            "the deadline releases a write that never reported back");
        A2APP.with(|state| { state.replace(previous_state); });
    }

    /// Withdrawing a held carried-over prompt must not advance the room's
    /// cursor: the message was never delivered, so the next timeline update
    /// re-forwards it (and re-raises the prompt).
    #[cfg(unix)]
    #[test]
    fn withdrawing_a_held_carried_prompt_leaves_the_cursor_for_refetch() {
        let previous_state = A2APP.with(|state| state.replace(None));
        initialize_state(AppRegistry::new(Vec::new()), PermissionStore::default(), A2AppPersistedState::default(), Default::default());
        let room_id: OwnedRoomId = SOURCE.try_into().unwrap();
        let prior: OwnedEventId = "$prior:example.org".try_into().unwrap();
        let held: OwnedEventId = "$held:example.org".try_into().unwrap();
        let context = a2app_core::information_flow::ContextId::Agent { account: "alice".into(), room: SOURCE.into() };
        let plan = TaskPlan {
            task_id: 1, subject: "s".into(), context, epoch: 1, title: "t".into(), explanation: "e".into(),
            items: Vec::new(), flow_dependencies: BTreeMap::new(), plan_hash: [0; 32], needs_fingerprint: [0; 32],
        };
        with_a2app(|state| {
            state.ai_rooms.insert(room_id.clone(), AiRoomInfo::new(Some(prior.clone())));
            state.task_prompts.push_back(TaskPrompt {
                room_id: room_id.clone(),
                plan,
                resume: TaskResume::UserPrompt { texts: vec![(held, "held message".into())] },
            });
        });
        assert_eq!(take_room_tasks(&room_id).len(), 1);
        assert_eq!(
            with_a2app(|state| state.ai_rooms.get(&room_id).and_then(|info| info.cursor.clone())).flatten(),
            Some(prior),
            "the held message's event must be re-forwarded on the next pass"
        );
        A2APP.with(|state| { state.replace(previous_state); });
    }

    /// A dead session's teardown must wait for its final Stopped row and Done
    /// snapshot: its flow context has to outlive them, so `retire_ai_session`
    /// defers and `settle_closed_turn` retires once the last write lands.
    #[cfg(unix)]
    #[test]
    fn a_dead_session_waits_for_its_final_writes_before_teardown() {
        let _published_grants = PublishedGrantsTestGuard::new();
        let previous_state = A2APP.with(|state| state.replace(None));
        initialize_state(AppRegistry::new(Vec::new()), PermissionStore::default(), A2AppPersistedState::default(), Default::default());
        let room_id: OwnedRoomId = SOURCE.try_into().unwrap();
        let context = a2app_core::information_flow::ContextId::Agent { account: "alice".into(), room: SOURCE.into() };
        with_a2app(|state| {
            state.ai_rooms.insert(room_id.clone(), AiRoomInfo::new(None));
            let info = state.ai_rooms.get_mut(&room_id).unwrap();
            info.final_writes_pending_revoke = true;
            info.pending_final_writes = BTreeSet::from([7u64]);
            state.task_ledger.insert(a2app_core::task_grants::AppliedTask {
                task_id: 13, subject: "agent".into(), context, epoch: 1,
                title: "t".into(), plan_hash: [0; 32], grants: Vec::new(), item_states: BTreeMap::new(),
            });
        });
        retire_ai_session(&room_id);
        assert!(with_a2app(|state| state.ai_rooms.get(&room_id).is_some_and(|info| info.retire_after_final_writes)).unwrap(),
            "the teardown waits for the final write");
        final_write_landed(&room_id, 7);
        assert!(with_a2app(|state| state.task_ledger.is_empty()).unwrap(),
            "the landed write releases the grants");
        assert!(with_a2app(|state| state.ai_rooms.get(&room_id).is_some_and(|info| !info.retire_after_final_writes)).unwrap(),
            "the deferred teardown ran once the write landed");
        A2APP.with(|state| { state.replace(previous_state); });
    }

    /// A stray late activity card cannot retain a closed turn's grants. The
    /// next legitimate member turn waits for this rollback before dispatch.
    #[cfg(unix)]
    #[test]
    fn a_late_activity_card_cannot_keep_closed_turn_grants_alive() {
        let _published_grants = PublishedGrantsTestGuard::new();
        let previous_state = A2APP.with(|state| state.replace(None));
        initialize_state(AppRegistry::new(Vec::new()), PermissionStore::default(), A2AppPersistedState::default(), Default::default());
        let room_id: OwnedRoomId = SOURCE.try_into().unwrap();
        let context = a2app_core::information_flow::ContextId::Agent { account: "alice".into(), room: SOURCE.into() };
        with_a2app(|state| {
            state.ai_rooms.insert(room_id.clone(), AiRoomInfo::new(None));
            let info = state.ai_rooms.get_mut(&room_id).unwrap();
            // A late notification opened a card while the final write drained.
            info.active_turn = Some(ActiveTurn {
                key: "turn-2".into(), tool_calls: Vec::new(), thinking: false, seq: 0, created_at: 0,
            });
            info.final_writes_pending_revoke = true;
            info.pending_final_writes = BTreeSet::from([5u64]);
            for task_id in [1u64, 2u64] {
                state.task_ledger.insert(a2app_core::task_grants::AppliedTask {
                    task_id,
                    subject: "agent".into(),
                    context: context.clone(),
                    epoch: 1,
                    title: "t".into(),
                    plan_hash: [0; 32],
                    grants: Vec::new(),
                    item_states: BTreeMap::new(),
                });
            }
        });
        final_write_landed(&room_id, 5);
        assert!(with_a2app(|state| state.task_ledger.is_empty()).unwrap(),
            "a late card must not extend the closed turn's permissions");
        A2APP.with(|state| { state.replace(previous_state); });
    }

    #[cfg(unix)]
    #[test]
    fn a_stopped_turn_completion_cannot_release_the_replacements_write_gate() {
        let fixture = FinalWriteTestState::new("stale-turn-write-result");
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let ui = WidgetRef::empty();
        fixture.close("old-turn");
        flush_pending_ai_turns(&mut cx);
        let old = with_a2app(|state| state.ai_rooms[&fixture.room].ai_turn_write_id.unwrap()).unwrap();
        stop_ai_session(&fixture.room);
        assert!(with_a2app(|state| state.ai_rooms[&fixture.room].ai_turn_write_id.is_none()).unwrap());

        fixture.close("replacement-turn");
        with_a2app(|state| state.ai_rooms.get_mut(&fixture.room).unwrap().last_ai_turn_post = None);
        flush_pending_ai_turns(&mut cx);
        let replacement = with_a2app(|state| {
            let info = state.ai_rooms.get_mut(&fixture.room).unwrap();
            info.ai_turn_backoff = Duration::from_secs(20);
            info.ai_turn_write_id.unwrap()
        }).unwrap();
        assert_ne!(old, replacement);
        for success in [true, false] {
            apply_ai_room_action(&mut cx, &ui, AiRoomAction::StateEventPosted {
                room_id: fixture.room.clone(), event_type: AI_TURN_EVENT_TYPE.into(), write_id: old, success,
            });
            with_a2app(|state| {
                let info = &state.ai_rooms[&fixture.room];
                assert!(info.ai_turn_in_flight);
                assert_eq!(info.ai_turn_write_id, Some(replacement));
                assert_eq!(info.anchor_in_flight.as_deref(), Some("replacement-turn"));
                assert_eq!(info.first_posted_turn.as_deref(), Some("replacement-turn"));
                assert_eq!(info.ai_turn_backoff, Duration::from_secs(20));
            });
        }
        // Ordinary activity completions still release their own final token,
        // while the replacement snapshot keeps the turn's coalescing gate.
        let activity = post_ai_state_event(&fixture.room, AI_ACTIVITY_EVENT_TYPE, "activity", &serde_json::json!({})).unwrap();
        register_final_write(&fixture.room, activity);
        apply_ai_room_action(&mut cx, &ui, AiRoomAction::StateEventPosted {
            room_id: fixture.room.clone(), event_type: AI_ACTIVITY_EVENT_TYPE.into(), write_id: activity, success: true,
        });
        assert!(with_a2app(|state| {
            let info = &state.ai_rooms[&fixture.room];
            info.ai_turn_in_flight && info.ai_turn_write_id == Some(replacement)
                && info.pending_final_writes == BTreeSet::from([replacement])
        }).unwrap());
        apply_ai_room_action(&mut cx, &ui, AiRoomAction::StateEventPosted {
            room_id: fixture.room.clone(), event_type: AI_TURN_EVENT_TYPE.into(), write_id: replacement, success: false,
        });
        with_a2app(|state| {
            let info = &state.ai_rooms[&fixture.room];
            assert!(!info.ai_turn_in_flight && info.ai_turn_write_id.is_none());
            assert!(info.anchor_in_flight.is_none() && info.first_posted_turn.is_none());
            assert_eq!(info.ai_turn_backoff, Duration::from_secs(40));
        });
    }

    #[cfg(unix)]
    #[test]
    fn a_snapshot_serialization_failure_releases_its_gate_and_anchor() {
        struct CannotSerialize;
        impl serde::Serialize for CannotSerialize {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("fixture failure"))
            }
        }
        let fixture = FinalWriteTestState::new("snapshot-serialization-failure");
        with_a2app(|state| {
            let info = state.ai_rooms.get_mut(&fixture.room).unwrap();
            info.ai_turn_in_flight = true;
            info.first_posted_turn = Some("failed-turn".into());
            info.anchor_in_flight = Some("failed-turn".into());
        });
        assert!(post_ai_state_event(&fixture.room, AI_TURN_EVENT_TYPE, "failed-turn", &CannotSerialize).is_none());
        with_a2app(|state| {
            let info = &state.ai_rooms[&fixture.room];
            assert!(!info.ai_turn_in_flight && info.ai_turn_write_id.is_none());
            assert!(info.anchor_in_flight.is_none() && info.first_posted_turn.is_none());
        });
    }

    #[cfg(unix)]
    #[test]
    fn member_messages_wait_for_final_rollback_before_a_fresh_carried_review() {
        use a2app_core::information_flow as flow;
        let fixture = FinalWriteTestState::new("member-turn-final-boundary");
        let provider = Recipient::ModelProvider("member-turn-provider".into());
        super::super::information_flow::ensure_agent_default_sharing(
            &fixture.context, SOURCE, Some("member-turn-provider"), None, &mut BTreeSet::new(),
        );
        let private = Source::Room { account: fixture.context.account().into(), room: TARGET.into() };
        flow::add_sources(&fixture.context, [private.clone()]).unwrap();
        let plan = build_carried_task_plan(&fixture.room, &fixture.context, &BTreeSet::from([private.clone()]),
            &provider, None, 2);
        let approved = plan.items.iter().map(|item| item.id.clone()).collect();
        let applied = task_grants::apply(&plan, &approved, &mut PermissionStore::default(), &task_grants::GlobalFlowApply).unwrap();
        with_a2app(|state| state.task_ledger.insert(applied));
        close_active_turn(&fixture.room, false);
        post_ai_reply(&fixture.room, "old turn's final reply".into(), None);
        let prior: OwnedEventId = "$prior:example.org".try_into().unwrap();
        let texts = vec![
            ("$first-held:example.org".try_into().unwrap(), "first next turn".into()),
            ("$second-held:example.org".try_into().unwrap(), "second next turn".into()),
        ];
        let token = with_a2app(|state| {
            let info = state.ai_rooms.get_mut(&fixture.room).unwrap();
            info.cursor = Some(prior.clone());
            queue_member_prompts(info, texts.clone());
            assert!(send_next_member_prompt(info, false, 0, |_| panic!("a new model prompt must wait for final writes")).is_none());
            assert_eq!(info.cursor, Some(prior.clone()));
            assert_eq!(info.pending_member_prompts.len(), 2);
            *info.pending_final_writes.first().unwrap()
        }).unwrap();
        assert!(with_a2app(|state| session_has_inflight_tool(state, &fixture.room)).unwrap(),
            "late host jobs must not spend the preceding turn's grants");
        assert!(flow::ensure_allowed(&fixture.context, &provider).is_ok());
        assert!(!hold_carried_permission_prompt(&fixture.room, &texts, &fixture.context, &provider, None, 3));
        final_write_landed(&fixture.room, token);
        assert!(flow::ensure_allowed(&fixture.context, &provider).is_err(), "the old turn's sharing was rolled back");
        assert!(!with_a2app(|state| session_has_inflight_tool(state, &fixture.room)).unwrap());
        assert!(hold_carried_permission_prompt(&fixture.room, &texts, &fixture.context, &provider, None, 3));
        with_a2app(|state| {
            let prompt = state.task_prompts.front().unwrap();
            assert!(prompt.plan.items.iter().any(|item| matches!(&item.action,
                task_grants::PlanAction::Flow { source, recipient } if source == &private && recipient == &provider)
                && item.state == ItemState::NeedsGrant));
            let TaskResume::UserPrompt { texts: held } = &prompt.resume else { panic!("held member prompt") };
            assert_eq!(held, &texts);
            assert_eq!(state.ai_rooms[&fixture.room].cursor, Some(prior));
        });
    }

    #[cfg(unix)]
    #[test]
    fn member_prompt_dispatch_sends_one_and_keeps_held_or_dead_messages_unforwarded() {
        let prior: OwnedEventId = "$prior:example.org".try_into().unwrap();
        let first: OwnedEventId = "$first:example.org".try_into().unwrap();
        let second: OwnedEventId = "$second:example.org".try_into().unwrap();
        let texts = vec![(first.clone(), "one".into()), (second.clone(), "two".into())];
        let mut info = AiRoomInfo::new(Some(prior.clone()));
        queue_member_prompts(&mut info, texts.clone());
        queue_member_prompts(&mut info, texts);
        assert_eq!(info.pending_member_prompts.len(), 2, "timeline rescans do not duplicate held asks");
        for (busy, queued) in [(true, 0), (false, 1)] {
            assert!(send_next_member_prompt(&mut info, busy, queued, |_| panic!("a turn or startup prompt is already queued")).is_none());
            assert_eq!(info.cursor, Some(prior.clone()));
        }
        assert!(send_next_member_prompt(&mut info, false, 0, |_| PromptOutcome::Dead).is_none());
        assert_eq!(info.cursor, Some(prior));
        assert_eq!(info.pending_member_prompts.len(), 2);
        let mut sent = Vec::new();
        assert!(send_next_member_prompt(&mut info, false, 0, |_| PromptOutcome::Queued).is_none());
        assert_eq!(info.pending_member_prompts.len(), 2, "a startup ask is not delivered until the transport sends it");
        assert_eq!(send_next_member_prompt(&mut info, false, 0, |text| { sent.push(text); PromptOutcome::Sent }), Some(first.clone()));
        assert_eq!(sent, vec!["one"]);
        assert_eq!(info.cursor, Some(first));
        assert_eq!(info.pending_member_prompts.front().unwrap().0, second);
        // An unexpected internal startup queue still prevents a second ask
        // from entering it before the host can review the next turn.
        assert!(send_next_member_prompt(&mut info, false, 1, |_| panic!("startup's first prompt must run alone")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn bounded_member_queues_and_carried_modals_keep_only_current_held_events() {
        let fixture = FinalWriteTestState::new("bounded-member-prompts");
        let limit = super::super::ai::session::MAX_QUEUED_PROMPTS;
        let texts = (0..limit + 3).map(|index|
            (format!("$held-{index}:example.org").try_into().unwrap(), index.to_string())).collect::<Vec<_>>();
        with_a2app(|state| {
            let info = state.ai_rooms.get_mut(&fixture.room).unwrap();
            queue_member_prompts(info, texts.clone());
            assert_eq!(info.pending_member_prompts.len(), limit);
            assert_eq!(info.cursor.as_ref(), Some(&texts[2].0), "only evicted asks advance the cursor");
            assert_eq!(info.pending_member_prompts.front().unwrap(), &texts[3]);
        });
        let provider = Recipient::ModelProvider("bounded-member-provider".into());
        a2app_core::information_flow::add_sources(&fixture.context, [Source::Room {
            account: fixture.context.account().into(), room: TARGET.into(),
        }]).unwrap();
        assert!(hold_carried_permission_prompt(&fixture.room, &texts, &fixture.context, &provider, None, 9));
        with_a2app(|state| {
            let TaskResume::UserPrompt { texts: held } = &state.task_prompts.front().unwrap().resume else { panic!("held prompt") };
            assert_eq!(held, &texts[3..]);
            let replacement = vec![("$latest:example.org".try_into().unwrap(), "latest".into())];
            assert!(update_pending_carried_texts(state, &fixture.room, &replacement));
            let TaskResume::UserPrompt { texts: held } = &state.task_prompts.front().unwrap().resume else { panic!("held prompt") };
            assert_eq!(held, &replacement, "evicted modal payloads cannot return to the host queue on approval");
        });
    }

    #[cfg(unix)]
    #[test]
    fn explicit_cancel_forgets_held_asks_but_session_death_retains_refetch_cursor() {
        let fixture = FinalWriteTestState::new("cancel-held-member-prompts");
        let prior: OwnedEventId = "$prior:example.org".try_into().unwrap();
        let held: OwnedEventId = "$cancel-held:example.org".try_into().unwrap();
        with_a2app(|state| {
            let info = state.ai_rooms.get_mut(&fixture.room).unwrap();
            info.cursor = Some(prior.clone());
            queue_member_prompts(info, vec![(held.clone(), "held".into())]);
            info.discard_session_work();
            assert_eq!(info.cursor, Some(prior.clone()), "a dead session leaves undelivered messages for refetch");
            queue_member_prompts(info, vec![(held.clone(), "held".into())]);
        });
        let mut cx = Cx::new(Box::new(|_, _| {}));
        abort_ai_room_work(&mut cx, &WidgetRef::empty(), &fixture.room);
        with_a2app(|state| {
            let info = &state.ai_rooms[&fixture.room];
            assert_eq!(info.cursor, Some(held));
            assert!(info.pending_member_prompts.is_empty());
        });
    }

    #[cfg(unix)]
    #[test]
    fn carried_review_checks_reply_recipients_even_with_durable_provider_sharing() {
        use a2app_core::information_flow as flow;
        let fixture = FinalWriteTestState::new("carried-reply-recipients");
        let provider = Recipient::ModelProvider("carried-reply-provider".into());
        let homeserver = Recipient::network_origin("https://carried-reply.example.org").unwrap();
        let own_room = Recipient::MatrixRoom { account: fixture.context.account().into(), room: SOURCE.into() };
        super::super::information_flow::ensure_agent_default_sharing(
            &fixture.context, SOURCE, Some("carried-reply-provider"), Some("https://carried-reply.example.org"),
            &mut BTreeSet::new(),
        );
        let private = Source::Room { account: fixture.context.account().into(), room: TARGET.into() };
        flow::add_sources(&fixture.context, [private.clone(), Source::UnknownPrivate]).unwrap();
        let texts = vec![("$carried-reply:test".try_into().unwrap(), "next turn".into())];
        for (missing, present) in [(&own_room, &homeserver), (&homeserver, &own_room)] {
            let provider_grant = flow::grant_sharing(private.clone(), provider.clone(),
                flow::ReaderScope::Context(fixture.context.clone()), flow::SharingDuration::Permanent).unwrap();
            let reply_grant = flow::grant_sharing(private.clone(), present.clone(),
                flow::ReaderScope::Context(fixture.context.clone()), flow::SharingDuration::RoomSession {
                    account: fixture.context.account().into(), room: SOURCE.into(),
                }).unwrap();
            assert!(flow::carried_over_sources(&fixture.context, &provider).unwrap().is_empty(),
                "provider-only discovery misses the withdrawn reply permission");
            assert!(hold_carried_permission_prompt(&fixture.room, &texts, &fixture.context, &provider, Some(&homeserver), 11));
            with_a2app(|state| {
                let prompt = state.task_prompts.pop_front().unwrap();
                assert_eq!(prompt.plan.items.len(), 3, "baseline and unknown sources keep their existing filtering");
                assert!(prompt.plan.items.iter().any(|item| matches!(&item.action,
                    task_grants::PlanAction::Flow { source, recipient } if source == &private && recipient == missing)
                    && item.state == ItemState::NeedsGrant));
                assert!(prompt.plan.items.iter().any(|item| matches!(&item.action,
                    task_grants::PlanAction::Flow { source, recipient } if source == &private && recipient == &provider)
                    && item.state == ItemState::AlreadyAllowed));
                let TaskResume::UserPrompt { texts: held } = prompt.resume else { panic!("host carried prompt") };
                assert_eq!(held, texts, "the member message is reviewed before forwarding");
            });
            flow::revoke_sharing(provider_grant).unwrap();
            flow::revoke_sharing(reply_grant).unwrap();
        }
    }
}
