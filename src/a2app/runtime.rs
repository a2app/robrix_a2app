//! The a2app runtime glue: owns all mini-app state (registry, grants,
//! prompts, the in-flight generation) and drives the host-service broker
//! once per event pass, mirroring how host_launcher's `App` did it.
//!
//! Widgets read this state through [`with_a2app`] and mutate it by emitting
//! [`A2AppOp`] actions, which [`process`] applies centrally.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use makepad_widgets::*;
use makepad_widgets::splash_host::SplashHostRequest;
use matrix_sdk::RoomState;
use matrix_sdk::ruma::{matrix_uri::MatrixId, MatrixToUri, MatrixUri, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId};

use a2app_core::builtin;
use a2app_core::bundle;
use a2app_core::manifest::{A2AppScope, AppRegistry, MiniAppId, MiniAppManifest};
use a2app_core::permissions::{
    Effective, GrantState, Permission, PermissionStore, agent_subject, is_agent_subject,
};
#[cfg(unix)]
use a2app_core::permissions::agent_room_of;
use a2app_core::persistence::{self, A2AppPersistedState};
use a2app_core::services::{
    self, Broker, BrokerAsk, BrokerCtx, HostAction, HostQuery, Reply, MATRIX_WRITE_OFF_MSG,
};
#[cfg(unix)]
use a2app_core::services::AppToolRequest;
use a2app_core::versions::{self, VersionOrigin};
use a2app_agent::intent::Intent;
use a2app_agent::pipeline::{GenOutcome, Generation};
use a2app_agent::prefs::AgentPrefs;

use crate::a2app::host_pane::{MiniAppHostPaneAction, MiniAppHostPaneWidgetRefExt};
use crate::a2app::permission_prompt::{
    MiniAppPermissionPromptWidgetRefExt, PermissionPromptAction, PromptInfo, ToolPreview,
};
use crate::a2app::dock::{DockCmd, PaneOp};
use crate::a2app::instances::{self, MiniAppInstanceAction, Surface};
use crate::a2app::matrix::{self, A2AppMatrixRequest, A2AppMatrixResult};
use crate::a2app::room_watch::{A2AppRoomWatchEvent, RoomWatchKind};
use crate::a2app::account_watch::{A2AppAccountWatchEvent, AccountWatchKind, ACCOUNT_HOOKS};
use a2app_core::layout::PaneLayout;
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
use crate::a2app::ai::session::{AiSession, PromptOutcome, SessionJob, SessionUpdate};
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
    /// The namespaced name the runtime routes by (also the app-tool key).
    pub full_name: String,
    /// The name the agent's turn card used for this call: the namespaced name
    /// when the model called the tool directly, or `call_mini_app_tool` when
    /// it came through the stable bridge. The finishing update must close the
    /// row the agent actually opened.
    pub display_name: String,
    pub answer: Sender<Result<String, String>>,
    pub since: Instant,
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
}

/// The state of the AI generation console shown in the Mini Apps screen.
#[derive(Default)]
pub struct GenConsole {
    pub status: String,
    pub lines: Vec<String>,
    /// True from submit until the user starts a new prompt.
    pub active: bool,
    pub last_render: Option<Instant>,
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
    /// The turn whose first `ai_turn` snapshot has been written: the card's
    /// anchor row, decided at flush time (see `flush_pending_ai_turns`).
    first_posted_turn: Option<String>,
    /// The turn whose anchor write is in flight, so a failed write can give
    /// the anchor back to the turn's next snapshot.
    anchor_in_flight: Option<String>,
    /// When starting this room's agent last failed, so a failure is retried
    /// after a cooldown instead of on every timeline update.
    start_failed_at: Option<Instant>,
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
            last_ai_turn_post: None,
            ai_turn_backoff: AI_TURN_POST_MIN_INTERVAL,
            status_busy: false,
            status_queued: 0,
            first_posted_turn: None,
            anchor_in_flight: None,
            start_failed_at: None,
        }
    }
}

/// All a2app state, owned by the UI thread.
pub struct A2AppState {
    pub registry: AppRegistry,
    pub permissions: PermissionStore,
    pub persisted: A2AppPersistedState,
    pub broker: Broker,
    pub prompts: VecDeque<PermissionPrompt>,
    pub active_prompt: Option<PermissionPrompt>,
    /// (app, permission) pairs the user said "Not Now" to this session.
    /// The key is the subject: an app id or an AI room's agent key.
    pub dismissed_prompts: HashSet<(String, Permission)>,
    /// (subject, host) pairs the user said "Not Now" to this session for the
    /// agent's internet tools. Kept separate from `dismissed_prompts` because
    /// network prompts are per HOST: dismissing one host must not silently
    /// refuse every other host for the rest of the session.
    pub dismissed_net_hosts: HashSet<(String, String)>,
    pub generation: Option<Generation>,
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
}

/// One-time startup: loads all persisted a2app state into the thread-local.
pub fn init() {
    a2app_core::set_data_root(crate::app_data_dir().join("a2app"));

    let mut registry = AppRegistry::new(builtin::builtin_apps());
    // A user's saved copy shadows the stock one; a built-in updated in this
    // build only reaches copies still following stock.
    for mut app in persistence::load_user_apps() {
        let following_stock = match &app.current_version {
            None => true,
            Some(stamp) => persistence::load_version(&app.id, stamp)
                .is_some_and(|(v, _)| v.origin == VersionOrigin::Stock),
        };
        if let Some(stock) = builtin::stock(&app.id)
            && app.builtin && following_stock && stock.source != app.source
        {
            app = on_stock(app, stock);
            if let Err(e) = persistence::save_user_app(&app) {
                error!("Failed to save the updated stock copy of {}: {e}", app.id);
            }
        }
        registry.insert(app);
    }
    let permissions = persistence::load_permissions();
    let persisted = persistence::load_registry_state();
    a2app_core::permissions::publish_snapshot(permissions.snapshot(&registry));

    A2APP.with(|state| {
        *state.borrow_mut() = Some(A2AppState {
            registry,
            permissions,
            persisted,
            broker: Broker::new(),
            prompts: VecDeque::new(),
            active_prompt: None,
            dismissed_prompts: HashSet::new(),
            dismissed_net_hosts: HashSet::new(),
            generation: None,
            console: GenConsole::default(),
            failed_request: None,
            agent_prefs: a2app_agent::prefs::load_agent_prefs(),
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
            generation_timer: None,
            #[cfg(unix)]
            ai_turn_flush_timer: None,
        });
    });
}

/// Operations that a2app widgets request; applied centrally in [`process`].
#[derive(Clone, Debug)]
pub enum A2AppOp {
    /// Opens (or brings back) an app. `room_id` attaches a room context
    /// (`None` falls back to the app's own scope); `in_room_pane` docks it
    /// into that room's RoomScreen pane instead of the generic host modal.
    OpenApp { app_id: MiniAppId, room_id: Option<OwnedRoomId>, in_room_pane: bool },
    CloseHostPane,
    ForceStop(MiniAppId),
    Uninstall(MiniAppId),
    ClearData(MiniAppId),
    Export(MiniAppId),
    ImportText(String),
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
    /// A single ability's own answer under its group (`Ask` = follow group).
    SetCapability { app_id: MiniAppId, cap_id: String, state: GrantState },
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
    /// The user's "Mini-apps can write to rooms" switch.
    SetMatrixWrite(bool),
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
    #[default]
    None,
}

struct PendingRoomAction {
    room_id: OwnedRoomId,
    action: RoomAction,
    since: Instant,
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
pub fn take_room_action(room_id: &RoomId) -> Option<RoomAction> {
    with_a2app(|state| {
        let pending = state.room_action.as_ref()?;
        if pending.room_id != room_id {
            return None;
        }
        let fresh = pending.since.elapsed() <= ROOM_ACTION_TTL;
        state.room_action.take().filter(|_| fresh).map(|p| p.action)
    }).flatten()
}



/// Drives all a2app machinery for one event pass. Called from
/// `App::handle_event` on every event; cheap early-outs keep it off the
/// hot path for events it doesn't care about.
pub fn process(cx: &mut Cx, ui: &WidgetRef, event: &Event) {
    if let Event::NetworkResponses(e) = event {
        with_a2app(|state| state.broker.handle_network(cx, e));
        instances::handle_network_responses(cx, event, &mut Scope::empty());
        return;
    }
    match event {
        Event::Signal | Event::Actions(_) | Event::Timer(_) => {}
        _ => return,
    }
    instances::flush_pending(cx);

    let mut ops: Vec<A2AppOp> = Vec::new();
    let mut prompt_answers: Vec<PermissionPromptAction> = Vec::new();
    let mut matrix_results: Vec<(Reply, Result<String, String>)> = Vec::new();
    let mut pane_actions: Vec<MiniAppHostPaneAction> = Vec::new();
    let mut stopped: Vec<MiniAppId> = Vec::new();
    let mut watch_events: Vec<A2AppRoomWatchEvent> = Vec::new();
    let mut account_events: Vec<A2AppAccountWatchEvent> = Vec::new();
    let mut host_events: Vec<(&'static str, serde_json::Value)> = Vec::new();
    #[cfg(unix)]
    let mut ai_room_actions: Vec<AiRoomAction> = Vec::new();
    #[cfg(unix)]
    let mut ai_panel_actions: Vec<AiRoomPanelAction> = Vec::new();
    if let Event::Actions(actions) = event {
        for action in actions {
            if let Some(watch_event) = action.downcast_ref::<A2AppRoomWatchEvent>() {
                watch_events.push(watch_event.clone());
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
            if let Some(answer) = action.downcast_ref::<PermissionPromptAction>() {
                prompt_answers.push(*answer);
                continue;
            }
            if let Some(result) = action.downcast_ref::<A2AppMatrixResult>() {
                matrix_results.push((result.reply, result.result.clone()));
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
                    _ => serde_json::json!({ "kind": "space" }),
                };
                host_events.push(("on_active_room_changed", payload));
                continue;
            }
            if let Some(nav) = action.downcast_ref::<NavigationBarAction>() {
                let (screen, space_id) = match nav {
                    NavigationBarAction::GoToHome => ("home", None),
                    NavigationBarAction::GoToAddRoom => ("add_room", None),
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
            if let Some(ai_room_action) = action.downcast_ref::<AiRoomAction>() {
                ai_room_actions.push(ai_room_action.clone());
            }
            #[cfg(unix)]
            if let Some(panel_action) = action.downcast_ref::<AiRoomPanelAction>() {
                ai_panel_actions.push(panel_action.clone());
            }
        }
    }

    // Deliver finished matrix calls back into their isolates. The callback
    // typically updates the app's UI, so schedule a repaint.
    let any_results = !matrix_results.is_empty();
    for (reply, result) in matrix_results {
        match &result {
            Ok(data) => services::respond(cx, reply, Ok(data.as_str())),
            Err(e) => services::respond(cx, reply, Err(e.as_str())),
        }
    }
    if any_results {
        // A callback's ui.X.render() output only commits on the NEXT event
        // pass, so queue one; the NextFrame keeps the paint loop ticking so
        // the commit actually PRESENTS instead of waiting for user input.
        SignalToUI::set_ui_signal();
        let _ = cx.new_next_frame();
        ui.redraw(cx);
    }

    for answer in prompt_answers {
        answer_permission_prompt(cx, ui, answer);
    }
    for pane_action in pane_actions {
        match pane_action {
            MiniAppHostPaneAction::CloseClicked => ops.push(A2AppOp::CloseHostPane),
            // The instance goes back to its room's dock, state intact.
            MiniAppHostPaneAction::ReturnToRoom { app_id, room_id } => {
                host_pane(cx, ui).close_active(cx, true);
                with_a2app(|state| state.foreground_app = None);
                ui.modal(cx, ids!(mini_app_host_modal)).close(cx);
                cx.action(DockCmd::Open { app_id, room_id });
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
    process_broker(cx, ui);

    // An action the user took that was refused or failed gets a popup that
    // says why, on top of the script's own error handling.
    for (app_id, error) in with_a2app(|state| state.broker.failures()).unwrap_or_default() {
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
    expire_timed_grants(cx, ui);
    persist_if_dirty();
}

fn host_pane(cx: &mut Cx, ui: &WidgetRef) -> crate::a2app::host_pane::MiniAppHostPaneRef {
    ui.mini_app_host_pane(cx, ids!(mini_app_host_modal.content))
}

/// One-time grants die with the app's last isolate.
fn app_stopped(cx: &mut Cx, app_id: &str) {
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

/// The dock position an instance last had, for its next open.
pub fn saved_layout(tag: &str) -> PaneLayout {
    with_a2app(|state| state.persisted.pane_layouts.get(tag).copied())
        .flatten()
        .unwrap_or_default()
}

pub fn remember_layout(tag: &str, layout: PaneLayout) {
    with_a2app(|state| {
        state.persisted.pane_layouts.insert(tag.to_string(), layout);
        state.registry_dirty = true;
    });
}

/// Forgets subscriptions whose isolate is gone or whose grant was pulled,
/// and starts or stops the worker's watches to match what's left.
fn prune_hook_subs() {
    with_a2app(|state| {
        let A2AppState { hook_subs, registry, permissions, watched_rooms, account_watched, .. } = state;
        hook_subs.retain(|heap, sub| {
            if instances::key_of_heap(*heap).is_none() {
                return false;
            }
            let Some(manifest) = registry.get(&sub.app_id) else { return false };
            sub.hooks.retain(|hook| {
                a2app_core::capabilities::for_hook(hook)
                    .is_some_and(|cap| permissions.effective_capability(manifest, cap) == Effective::Granted)
            });
            !sub.hooks.is_empty()
        });
        let wanted: HashSet<OwnedRoomId> = hook_subs.values().filter_map(|s| s.room_id.clone()).collect();
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
    // Messages and receipts batch per pass, reactions, edits and invites get
    // a call each, the rest coalesce to the latest; a `None` room is account-wide.
    let mut messages: HashMap<OwnedRoomId, Vec<serde_json::Value>> = HashMap::new();
    let mut receipts: HashMap<OwnedRoomId, Vec<serde_json::Value>> = HashMap::new();
    let mut each: Vec<(Option<OwnedRoomId>, &'static str, serde_json::Value)> = Vec::new();
    let mut latest: HashMap<(Option<OwnedRoomId>, &'static str), serde_json::Value> = HashMap::new();
    let mut closed: Vec<OwnedRoomId> = Vec::new();
    for event in events {
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
                each.push((Some(room_id.clone()), "on_room_reaction", serde_json::json!({
                    "room_id": room_id,
                    "event_id": event_id,
                    "key": key,
                    "sender_id": sender,
                    "added": added,
                })));
            }
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
    let subs: Vec<(usize, Option<OwnedRoomId>, HashSet<&'static str>)> = with_a2app(|state| {
        state.hook_subs.iter().map(|(heap, s)| (*heap, s.room_id.clone(), s.hooks.clone())).collect()
    }).unwrap_or_default();
    let mut delivered = false;
    for (heap, room_id, hooks) in subs {
        if let Some(room_id) = &room_id {
            if hooks.contains("on_room_message")
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
            delivered |= instances::call_hook_by_heap(cx, heap, LiveId::from_str(hook), &[&payload.to_string()]);
        }
        for ((_, hook), payload) in latest.iter().filter(|((room, hook), _)| mine(room, hook)) {
            delivered |= instances::call_hook_by_heap(cx, heap, LiveId::from_str(hook), &[&payload.to_string()]);
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

/// Drops every instance of the app, on every surface.
fn stop_app_everywhere(cx: &mut Cx, ui: &WidgetRef, app_id: &str) {
    host_pane(cx, ui).drop_app(cx, app_id);
    cx.action(DockCmd::QuitEverywhere(app_id.to_string()));
    instances::quit_app(cx, app_id);
}

fn apply_op(cx: &mut Cx, ui: &WidgetRef, op: A2AppOp) {
    match op {
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
            if restricted {
                enqueue_popup_notification(
                    format!("\"{}\" was stopped for hammering the host with requests. You can let it run again from its app info.", manifest.name),
                    PopupKind::Warning, Some(6.0),
                );
                return;
            }
            match (in_room_pane, room) {
                // One isolate per (app, room): the target room's dock opens
                // (or restores) ITS OWN instance, independent of any other.
                (true, Some(pane_room)) => {
                    cx.action(DockCmd::Open { app_id, room_id: pane_room });
                }
                (_, room) => {
                    if host_pane(cx, ui).open_app(cx, &manifest, grants, room.clone()) {
                        with_a2app(|state| state.foreground_app = Some(app_id.clone()));
                        ui.modal(cx, ids!(mini_app_host_modal)).open(cx);
                    } else if let Some(room_id) = room {
                        // Shown in a room already: bring that up instead.
                        cx.action(DockCmd::Open { app_id, room_id });
                    }
                }
            }
        }
        A2AppOp::CloseHostPane => {
            // Close QUITS: keeping apps alive is what minimize is for.
            with_a2app(|state| state.foreground_app = None);
            if let Some((app_id, _)) = host_pane(cx, ui).close_active(cx, false) {
                app_stopped(cx, &app_id);
            }
            ui.modal(cx, ids!(mini_app_host_modal)).close(cx);
            ui.redraw(cx);
        }
        A2AppOp::ForceStop(app_id) => {
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
            let Some(Some(manifest)) = with_a2app(|state| state.registry.get(&app_id).cloned()) else { return };
            if manifest.builtin {
                enqueue_popup_notification("Built-in mini-apps can't be uninstalled.", PopupKind::Warning, Some(4.0));
                return;
            }
            stop_app_everywhere(cx, ui, &app_id);
            with_a2app(|state| {
                // A generated/imported app exists nowhere else; keep the
                // manifest so uninstall isn't destruction.
                state.persisted.archived.retain(|a| a.id != app_id);
                state.persisted.archived.push(manifest.clone());
                state.registry.remove(&app_id);
                state.permissions.remove_app(&app_id);
                state.foreground_app.take_if(|f| *f == app_id);
                state.perms_dirty = true;
                state.registry_dirty = true;
            });
            persistence::remove_user_app(&app_id);
            persistence::clear_app_data(&app_id);
            publish_grants(cx);
            cx.action(A2AppRuntimeAction::Uninstalled(app_id.clone()));
            enqueue_popup_notification(
                format!("Uninstalled \"{}\". Its bundle was archived.", manifest.name),
                PopupKind::Success, Some(4.0),
            );
            ui.redraw(cx);
        }
        A2AppOp::ClearData(app_id) => {
            persistence::clear_app_data(&app_id);
            enqueue_popup_notification("Cleared this mini-app's saved data.", PopupKind::Success, Some(3.0));
            ui.redraw(cx);
        }
        A2AppOp::Export(app_id) => {
            let Some(Some(manifest)) = with_a2app(|state| state.registry.get(&app_id).cloned()) else { return };
            let text = bundle::to_text(&manifest);
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
        A2AppOp::ImportText(text) => install_import(cx, ui, bundle::parse(&text)),
        A2AppOp::ImportFile(path) => {
            let parsed = std::fs::read_to_string(&path)
                .map_err(|e| format!("Couldn't read that file: {e}"))
                .and_then(|text| bundle::parse(&text));
            install_import(cx, ui, parsed);
        }
        A2AppOp::SwitchVersion { app_id, stamp } => {
            let switched = with_a2app(|state| {
                let mut manifest = state.registry.get(&app_id).cloned()?;
                let (version, source) = persistence::load_version(&app_id, &stamp)?;
                archive_current(&mut manifest);
                let label = versions::label_for(version.at_unix, utc_offset_secs());
                Some((version.apply_to(&manifest, source), label))
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
            let saved = with_a2app(|state| {
                let mut base = state.registry.get(&app_id).cloned()?;
                if base.source == source {
                    return Some(None);
                }
                archive_current(&mut base);
                let mut updated = a2app_core::manifest::rewritten(&base, source);
                commit_version(&mut updated, VersionOrigin::Manual, "Edited by hand");
                Some(Some(updated))
            }).flatten();
            match saved {
                Some(Some(updated)) => install_version(cx, ui, updated, String::from("Saved your edit as a new version.")),
                Some(None) => enqueue_popup_notification("Nothing changed.", PopupKind::Info, Some(3.0)),
                None => enqueue_popup_notification("That mini-app no longer exists.", PopupKind::Error, Some(4.0)),
            }
        }
        A2AppOp::ResetToStock(app_id) => {
            let reset = with_a2app(|state| {
                let current = state.registry.get(&app_id).cloned()?;
                let stock = builtin::stock(&app_id)?;
                if current.source == stock.source {
                    return Some(None);
                }
                Some(Some(on_stock(current, stock)))
            }).flatten();
            match reset {
                Some(Some(updated)) => install_version(cx, ui, updated, String::from("Back on the stock version.")),
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
                    .add_file_with_mime_type(&path, "application/json")
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
            if let Err(e) = queue_room_action(cx, room_id, RoomAction::OpenApp(app_id)) {
                enqueue_popup_notification(format!("Couldn't open it there: {e}"), PopupKind::Error, Some(5.0));
            }
        }
        A2AppOp::SetPermission { app_id, perm, state: new_state } => {
            with_a2app(|state| {
                state.permissions.set(&app_id, perm, new_state);
                state.perms_dirty = true;
            });
            publish_grants(cx);
            apply_permission_to_running(cx, ui, &app_id, perm);
            ui.redraw(cx);
        }
        A2AppOp::SetCapability { app_id, cap_id, state: new_state } => {
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
            let retry = with_a2app(|state| state.failed_request.take()).flatten();
            if let Some(request) = retry {
                start_generation(cx, ui, request, None, None);
            }
        }
        A2AppOp::NewPrompt => {
            with_a2app(|state| {
                state.console = GenConsole::default();
                state.failed_request = None;
            });
            ui.redraw(cx);
        }
        A2AppOp::SetMatrixWrite(on) => {
            let senders: Vec<MiniAppId> = with_a2app(|state| {
                state.permissions.set_matrix_write(on);
                state.perms_dirty = true;
                state.registry.iter()
                    .filter(|m| m.declares(Permission::MatrixRoomSend) && instances::is_running(&m.id))
                    .map(|m| m.id.clone())
                    .collect()
            }).unwrap_or_default();
            publish_grants(cx);
            for app_id in senders {
                apply_permission_to_running(cx, ui, &app_id, Permission::MatrixRoomSend);
            }
            enqueue_popup_notification(
                if on { "Mini-apps may now write to rooms. Each one still asks you first." }
                else { "Mini-apps can no longer write to rooms." },
                PopupKind::Info, Some(4.0),
            );
            ui.redraw(cx);
        }
        A2AppOp::ShareToRoom { app_id, room_id } => {
            if !with_a2app(|state| state.permissions.matrix_write()).unwrap_or(false) {
                enqueue_popup_notification(
                    format!("Sharing an app into a room is blocked: {MATRIX_WRITE_OFF_MSG}."),
                    PopupKind::Warning, Some(5.0),
                );
                return;
            }
            let Some(Some(manifest)) = with_a2app(|state| state.registry.get(&app_id).cloned()) else { return };
            submit_async_request(MatrixRequest::A2App(A2AppMatrixRequest::ShareApp {
                room_id,
                bundle_json: bundle::to_text(&manifest),
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

fn install_import(cx: &mut Cx, ui: &WidgetRef, parsed: Result<MiniAppManifest, String>) {
    match parsed {
        Ok(mut manifest) => {
            with_a2app(|state| {
                // An import can never overwrite an app you already have.
                let taken: Vec<MiniAppId> = state.registry.iter().map(|a| a.id.clone()).collect();
                manifest.id = unique_import_id(&manifest.id, &taken);
                if let Err(e) = persistence::save_user_app(&manifest) {
                    error!("Failed to save imported mini-app: {e}");
                }
                state.registry.insert(manifest.clone());
            });
            publish_grants(cx);
            enqueue_popup_notification(
                format!("Installed \"{}\".", manifest_name_for_popup(&manifest)),
                PopupKind::Success, Some(4.0),
            );
            ui.redraw(cx);
        }
        Err(e) => enqueue_popup_notification(format!("Import failed: {e}"), PopupKind::Error, Some(5.0)),
    }
}

fn manifest_name_for_popup(manifest: &MiniAppManifest) -> String {
    if manifest.name.is_empty() { manifest.id.clone() } else { manifest.name.clone() }
}

fn unique_import_id(base: &str, taken: &[MiniAppId]) -> MiniAppId {
    if !taken.iter().any(|t| t == base) {
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if !taken.contains(&candidate) {
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
    with_a2app(|state| {
        if state.generation.is_some() {
            enqueue_popup_notification("A generation is already running.", PopupKind::Warning, Some(3.0));
            return;
        }
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
        let generation = match refine_target.and_then(|id| state.registry.get(&id).cloned()) {
            Some(mut base) => {
                // The state being rewritten stays reachable as a version.
                archive_current(&mut base);
                state.registry.insert(base.clone());
                Generation::start_refine(request.clone(), base, state.agent_prefs.clone())
            }
            None => Generation::start(request.clone(), taken, scope, state.agent_prefs.clone()),
        };
        match generation {
            Ok(generation) => {
                state.generation = Some(generation);
                state.console = GenConsole {
                    status,
                    lines: Vec::new(),
                    active: true,
                    last_render: None,
                };
                state.failed_request = Some(request);
            }
            Err(e) => {
                state.console.status = e.clone();
                state.console.active = true;
                enqueue_popup_notification(e, PopupKind::Error, Some(6.0));
            }
        }
    });
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
                state.console.status = format!("Failed: {reason}");
                Some(Done::Failed(reason))
            }
        }
    }).flatten();

    match done {
        Some(Done::Ready { manifest, refine_of }) => {
            let mut manifest = *manifest;
            with_a2app(|state| {
                if refine_of.is_none() {
                    if let Some(room) = state.create_room.take() {
                        manifest.scope = A2AppScope::Room { room_id: room.to_string() };
                    }
                }
                let request = state.generation.as_ref().map(|g| g.request().to_string()).unwrap_or_default();
                commit_version(&mut manifest, VersionOrigin::Ai, &request);
                if let Err(e) = persistence::save_user_app(&manifest) {
                    error!("Failed to save generated mini-app: {e}");
                }
                state.registry.insert(manifest.clone());
                state.generation = None;
                state.failed_request = None;
                state.console.status = match refine_of {
                    Some(_) => format!("Updated \"{}\".", manifest.name),
                    None => format!("Created \"{}\".", manifest.name),
                };
                state.registry_dirty = true;
            });
            publish_grants(cx);
            // If the old version was running, quit it so the next open boots
            // the new source (the reopen_hint below says so); then, if a
            // session's launch_splash_app tool call started this run, answer
            // it with the installed summary and dock the app into the room it
            // was asked for.
            let was_running = stop_for_restart(cx, ui, &manifest);
            #[cfg(unix)]
            {
                let summary = serde_json::json!({
                    "app_id": manifest.id,
                    "name": manifest.name,
                    "status": "installed_and_running",
                });
                resolve_session_generation(cx, ui, Some(&manifest), Ok(summary.to_string()));
            }
            enqueue_popup_notification(
                reopen_hint(format!("Mini-app \"{}\" is ready.", manifest.name), was_running),
                PopupKind::Success, Some(5.0),
            );
            ui.redraw(cx);
        }
        Some(Done::Failed(reason)) => {
            with_a2app(|state| state.generation = None);
            #[cfg(unix)]
            resolve_session_generation(cx, ui, None, Err(format!("The build failed: {reason}")));
            ui.redraw(cx);
        }
        None => {}
    }
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

fn process_broker(cx: &mut Cx, ui: &WidgetRef) {
    let asks = with_a2app(|state| {
        let A2AppState { broker, registry, permissions, foreground_app, .. } = state;
        let is_docked = |app_id: &str| instances::is_docked(app_id);
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
            room_name: &room_name,
            desktop_view,
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
        BrokerAsk::IpcDeliver { reply, from, from_heap, to, data_json } => {
            let delivered = instances::deliver_ipc(cx, from_heap, &from, &to, &data_json);
            let body = format!("{{\"delivered\":{delivered}}}");
            services::respond(cx, reply, Ok(&body));
        }
        BrokerAsk::Matrix { reply, app_id, room, call } => {
            let room = room.and_then(|r| OwnedRoomId::try_from(r.as_str()).ok());
            match matrix::request_for(call, room, reply) {
                Ok(request) => submit_async_request(MatrixRequest::A2App(request)),
                Err(e) => {
                    with_a2app(|state| state.broker.note(reply, &app_id));
                    services::respond(cx, reply, Err(e));
                }
            }
        }
        BrokerAsk::Subscribe { reply, app_id, heap_key, room, hook } => {
            let Ok(room_id) = room.as_deref().map(OwnedRoomId::try_from).transpose() else {
                return services::respond(cx, reply, Err("not a valid room id"));
            };
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
            // Only the modal's own isolate has to get out of the way when it
            // sends the user elsewhere; closing quits it, like its Close button.
            let leaves_modal = !matches!(action, HostAction::OpenApp { .. })
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
                apply_op(cx, ui, A2AppOp::CloseHostPane);
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

/// Withdraws one tool, but only for the instance that registered it.
#[cfg(unix)]
fn unregister_app_tool(
    cx: &mut Cx,
    reply: Reply,
    app_id: &str,
    heap_key: usize,
    full_name: &str,
) {
    let removed = with_a2app(|state| {
        let owner = state
            .app_tools
            .get(full_name)
            .map(|reg| (reg.heap_key, reg.app_id.clone(), reg.room_id.clone()));
        match owner {
            Some((reg_heap, reg_app, room)) if reg_heap == heap_key && reg_app == app_id => {
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
        Some(p) if p.app_id == app_id => state.app_tool_calls.remove(&call_id),
        _ => None,
    })
    .flatten();
    let Some(pending) = pending else {
        return services::respond(cx, reply, Err("no matching tool call for this app"));
    };
    let summary: String = text.chars().take(200).collect();
    let detail = finish_ai_tool_call(&pending.room_id, &pending.display_name, ok, &summary);
    push_ai_tool_receipt(&pending.room_id, &pending.display_name, detail, ok, &summary);
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
    let destination_room = BasicRoomDetails::Name(joined_room_name(cx, &room_id)?);
    with_a2app(|state| {
        state.room_action = Some(PendingRoomAction {
            room_id: room_id.clone(),
            action,
            since: Instant::now(),
        });
    });
    cx.action(NavigationBarAction::GoToHome);
    cx.action(AppStateAction::NavigateToRoom { room_to_close: None, destination_room });
    cx.action(A2AppRoomAction::Pending { room_id });
    Ok(())
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
fn commit_version(manifest: &mut MiniAppManifest, origin: VersionOrigin, note: &str) {
    let parent = manifest.current_version.take();
    let version = versions::new_version(
        manifest, origin, note, parent.as_deref(), versions::now_unix(), utc_offset_secs(),
    );
    match persistence::append_version(manifest, version) {
        Ok(stamp) => manifest.current_version = Some(stamp),
        Err(e) => {
            error!("Failed to archive a version of {}: {e}", manifest.id);
            manifest.current_version = parent;
        }
    }
}

/// Before the working copy gets replaced: makes sure it is a version, so it
/// stays reachable. A pristine built-in archives as its stock version.
fn archive_current(manifest: &mut MiniAppManifest) {
    let pristine = manifest.builtin
        && builtin::stock(&manifest.id).is_some_and(|stock| stock.source == manifest.source);
    let (origin, note) = if pristine {
        (VersionOrigin::Stock, "Stock")
    } else {
        (VersionOrigin::Legacy, "Before version history")
    };
    if let Err(e) = persistence::ensure_current_version(
        manifest, origin, note, versions::now_unix(), utc_offset_secs(),
    ) {
        error!("Failed to archive the current version of {}: {e}", manifest.id);
    }
}

/// `current` put on this build's stock source. An archived stock version
/// with that exact source is reused rather than duplicated.
fn on_stock(mut current: MiniAppManifest, mut stock: MiniAppManifest) -> MiniAppManifest {
    archive_current(&mut current);
    let archived = persistence::list_versions(&current.id).into_iter()
        .filter(|v| v.origin == VersionOrigin::Stock)
        .filter_map(|v| persistence::load_version(&current.id, &v.stamp))
        .find(|(_, source)| *source == stock.source);
    match archived {
        Some((version, source)) => version.apply_to(&current, source),
        None => {
            stock.scope = current.scope;
            stock.current_version = current.current_version;
            commit_version(&mut stock, VersionOrigin::Stock, "Stock");
            stock
        }
    }
}

/// Makes `updated` the app's working copy: saved, registered, restarted
/// where it runs, and announced.
fn install_version(cx: &mut Cx, ui: &WidgetRef, updated: MiniAppManifest, done: String) {
    if let Err(e) = persistence::save_user_app(&updated) {
        error!("Failed to save mini-app {}: {e}", updated.id);
    }
    with_a2app(|state| state.registry.insert(updated.clone()));
    publish_grants(cx);
    let was_running = stop_for_restart(cx, ui, &updated);
    cx.action(A2AppRuntimeAction::VersionsChanged(updated.id.clone()));
    enqueue_popup_notification(reopen_hint(done, was_running), PopupKind::Success, Some(5.0));
    ui.redraw(cx);
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
                "add_room" => NavigationBarAction::GoToAddRoom,
                "mini_apps" => NavigationBarAction::GoToMiniApps,
                "settings" => NavigationBarAction::OpenSettings,
                _ => return Err(String::from("screen must be one of home, add_room, mini_apps, settings")),
            };
            cx.action(action);
        }
        HostAction::OpenLink { room, url } => {
            let matrix_id = MatrixToUri::parse(&url).map(|u| u.id().clone())
                .or_else(|_| MatrixUri::parse(&url).map(|u| u.id().clone()))
                .map_err(|_| String::from("not a matrix.to or matrix: link"))?;
            let action = match matrix_id {
                MatrixId::User(user_id) => HostAction::ShowUser { room, user_id: user_id.to_string() },
                MatrixId::Room(room_id) => HostAction::OpenRoom { room: room_id.to_string() },
                MatrixId::Event(room_or_alias, event_id) => {
                    let room_id = OwnedRoomId::try_from(room_or_alias)
                        .map_err(|_| String::from("room aliases can't be resolved yet"))?;
                    HostAction::JumpToEvent { room: Some(room_id.to_string()), event_id: event_id.to_string() }
                }
                _ => return Err(String::from("room aliases can't be resolved yet")),
            };
            return perform_host_action(cx, ui, heap, action);
        }
        HostAction::OpenApp { room, app_id } => {
            let installed = with_a2app(|state| {
                state.registry.get(&app_id).map(|_| state.permissions.is_restricted(&app_id))
            }).flatten();
            match installed {
                None => return Err(String::from("no such app")),
                Some(true) => return Err(String::from("that app is stopped for hammering the host")),
                Some(false) => {}
            }
            let room_id = room.and_then(|r| OwnedRoomId::try_from(r.as_str()).ok());
            let in_room_pane = room_id.is_some();
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
            queue_room_action(cx, room_id, RoomAction::InsertDraft(text))?;
        }
        HostAction::ComposerReplyTo { room, event_id } => {
            let room_id = room_of(room)?;
            queue_room_action(cx, room_id, RoomAction::ReplyTo(event_of(&event_id)?))?;
        }
        HostAction::ClosePane | HostAction::SetSide { .. } | HostAction::Minimize | HostAction::BreakOut => {
            let key = instances::key_of_heap(heap).ok_or("this instance has no pane")?;
            let desktop = effective_is_desktop(cx);
            let op = match (&action, instances::surface_of(&key)) {
                // The caller closes the modal once this answers.
                (HostAction::ClosePane, Some(Surface::Modal)) => return Ok(()),
                (HostAction::ClosePane, None) => {
                    if instances::quit(cx, &key) {
                        cx.action(MiniAppInstanceAction::AppStopped(key.0));
                    }
                    return Ok(());
                }
                (HostAction::ClosePane, _) => PaneOp::Close,
                (HostAction::BreakOut, Some(Surface::Tab)) => return Ok(()),
                (HostAction::SetSide { side }, Some(Surface::Dock)) => PaneOp::SetSide(*side),
                (HostAction::Minimize, Some(Surface::Dock)) => PaneOp::Minimize,
                (HostAction::BreakOut, Some(Surface::Dock)) if desktop => PaneOp::BreakOut,
                (HostAction::BreakOut, Some(Surface::Dock)) => {
                    return Err(String::from("breaking out needs the desktop layout"));
                }
                _ => return Err(String::from("this instance is not docked in a room")),
            };
            let (app_id, room_id) = key;
            let room_id = room_id.ok_or("this instance is not docked in a room")?;
            cx.action(DockCmd::Pane { app_id, room_id, op });
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
    let dismissed = with_a2app(|state| state.dismissed_prompts.contains(&(subject.clone(), perm)))
        .unwrap_or(false);
    if dismissed {
        refuse_parked_request(cx, perm, parked);
        return;
    }
    with_a2app(|state| {
        // Merge into an already-active or queued prompt for the same pair.
        // Network prompts are per HOST and tool prompts per TOOL: two
        // different hosts/tools must never share a prompt, or one answer
        // would decide for both.
        let new_host = match &parked {
            #[cfg(unix)]
            ParkedRequest::NetworkAccess { host, .. } => Some(host.clone()),
            _ => None,
        };
        let new_tool = tool.as_ref().map(|t| t.full_name.clone());
        let same = |p: &PermissionPrompt| {
            if p.subject != subject || p.perm != perm {
                return false;
            }
            let existing_host = p.parked.iter().find_map(|q| match q {
                #[cfg(unix)]
                ParkedRequest::NetworkAccess { host, .. } => Some(host.clone()),
                _ => None,
            });
            let existing_tool = p.tool.as_ref().map(|t| t.full_name.clone());
            existing_host == new_host && existing_tool == new_tool
        };
        if let Some(active) = state.active_prompt.as_mut().filter(|p| same(p)) {
            active.parked.push(parked);
            return;
        }
        if let Some(queued) = state.prompts.iter_mut().find(|p| same(p)) {
            queued.parked.push(parked);
            return;
        }
        state.prompts.push_back(PermissionPrompt {
            subject,
            perm,
            parked: vec![parked],
            tool,
        });
    });
    show_next_permission_prompt(cx, ui);
}

/// The tool name an AI session job maps to (the name the model called).
#[cfg(unix)]
fn ai_job_tool_name(job: &SessionJob) -> String {
    match job {
        SessionJob::ReadTool { kind, .. } => read_tool_name(kind).to_string(),
        SessionJob::LaunchSplashApp { .. } => String::from("launch_splash_app"),
        SessionJob::ListApps { .. } => String::from("list_apps"),
        SessionJob::LaunchApp { .. } => String::from("launch_app"),
        SessionJob::SendRoomMessage { .. } => String::from("send_message"),
        SessionJob::PostRoomMessage { .. } => String::from("post_room_message"),
        // Not a tool call: the agent's web tool is already shown by its ACP
        // events; this job only asks whether a host may be reached.
        SessionJob::NetworkAccess { .. } => String::from("network_access"),
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
        // No target of its own: the ACP `tool_call` event names the web tool,
        // and the turn card is reposted with that name; this job carries only
        // the host the user is being asked about.
        SessionJob::NetworkAccess { .. } => None,
        // The tool name already names it; no extra target to add.
        SessionJob::InvokeMiniAppTool { .. } => None,
        // A bridged call still targets one app tool; name it for the card.
        SessionJob::CallMiniAppTool { tool, .. } => with_a2app(|state| {
            state.app_tools.get(tool).map(|reg| format!("“{}”", reg.raw_name))
        })
        .flatten(),
        // A listing has no single target of its own.
        SessionJob::ListMiniAppTools { .. } => None,
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
            note_ai_tool_call(&room_id, &ai_job_tool_name(&job), false, &reason);
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
    let subject = agent_subject(room_id.as_str());
    let (parked, was_active) = with_a2app(|state| {
        let mut parked: Vec<(Permission, ParkedRequest)> = Vec::new();
        let mut was_active = false;
        if state.active_prompt.as_ref().is_some_and(|p| p.subject == subject) {
            if let Some(p) = state.active_prompt.take() {
                let perm = p.perm;
                parked.extend(p.parked.into_iter().map(|q| (perm, q)));
                was_active = true;
            }
        }
        let (mine, rest): (Vec<_>, Vec<_>) =
            state.prompts.drain(..).partition(|p| p.subject == subject);
        state.prompts = rest.into_iter().collect();
        for p in mine {
            let perm = p.perm;
            parked.extend(p.parked.into_iter().map(|q| (perm, q)));
        }
        (parked, was_active)
    })
    .unwrap_or_default();
    for (perm, request) in parked {
        refuse_parked_request(cx, perm, request);
    }
    if was_active {
        ui.modal(cx, ids!(a2app_permission_modal)).close(cx);
        show_next_permission_prompt(cx, ui);
    }
}

fn show_next_permission_prompt(cx: &mut Cx, ui: &WidgetRef) {
    let rooms = cx.has_global::<RoomsListRef>().then(|| cx.get_global::<RoomsListRef>().clone());
    let info = with_a2app(|state| {
        if state.active_prompt.is_some() {
            return None;
        }
        let prompt = state.prompts.pop_front()?;
        let info = prompt_info_for(
            state,
            rooms.as_ref(),
            &prompt.subject,
            prompt.perm,
            &prompt.parked,
            prompt.tool.as_ref(),
        );
        state.active_prompt = Some(prompt);
        Some(info)
    })
    .flatten();
    let Some(info) = info else { return };
    ui.mini_app_permission_prompt(cx, ids!(a2app_permission_modal.content)).show(cx, &info);
    ui.modal(cx, ids!(a2app_permission_modal)).open(cx);
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
                SessionJob::NetworkAccess { .. } => continue,
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
                "The AI wants to fetch from “{host}”. Allowing covers exactly this site; \
                 it will ask again before reaching any other."
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
                SessionJob::NetworkAccess { .. } => continue,
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
            app_name: format!("AI in {room_name}"),
            app_icon: String::from("🤖"),
            perm,
            reason: Some(ai_prompt_reason(rooms, perm, parked)),
            capability: Some(ai_prompt_action(rooms, perm, parked)),
            agent: true,
            tool: tool_preview,
        };
    }
    let (app_name, app_icon, reason) = state.registry.get(subject)
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
        .map(|c| c.title.to_string())
    };
    PromptInfo { app_name, app_icon, perm, reason, capability, agent: false, tool: tool_preview }
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
        | SessionJob::InvokeMiniAppTool { answer, .. }
        | SessionJob::CallMiniAppTool { answer, .. }
        | SessionJob::ListMiniAppTools { answer } => {
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
        "The user did not allow the AI in this room to do this (\"{}\"). \
         Tell them what you wanted to do and why, so they can allow it.",
        perm.title()
    )
}

fn answer_permission_prompt(cx: &mut Cx, ui: &WidgetRef, answer: PermissionPromptAction) {
    ui.modal(cx, ids!(a2app_permission_modal)).close(cx);
    let Some(Some(prompt)) = with_a2app(|state| state.active_prompt.take()) else { return };

    // Internet-access prompts are answered per HOST, not per group: an Allow
    // records only that host (or a session-only version of it), and a Deny
    // refuses this host without turning the whole internet off (or setting a
    // durable group Denied that would silently block every other host).
    let network_hosts: Vec<String> = prompt
        .parked
        .iter()
        .filter_map(|p| match p {
            #[cfg(unix)]
            ParkedRequest::NetworkAccess { host, .. } => Some(host.clone()),
            _ => None,
        })
        .collect();

    // A prompt for a cross-room post/read decides ONE room: its answer must
    // not write the group grant, or one Allow would unlock every room (the
    // group `Granted` is the panel's explicit "all rooms").
    #[cfg(unix)]
    let per_room = !prompt.parked.is_empty()
        && prompt.parked.iter().all(|p| matches!(p,
            ParkedRequest::AiTool {
                job: SessionJob::PostRoomMessage { .. }
                    | SessionJob::ReadTool { kind: ReadToolKind::OtherRoom { .. }, .. },
                ..
            }));
    #[cfg(not(unix))]
    let per_room = false;
    let granted = if !network_hosts.is_empty() {
        match answer {
            PermissionPromptAction::Allow => {
                with_a2app(|state| {
                    for host in &network_hosts {
                        state.permissions.allow_host(&prompt.subject, host);
                    }
                    state.perms_dirty = true;
                });
                true
            }
            PermissionPromptAction::AllowOnce => {
                with_a2app(|state| {
                    for host in &network_hosts {
                        state.permissions.allow_host_once(&prompt.subject, host);
                    }
                });
                true
            }
            // A refusal covers exactly this host, so the group is left as it
            // was: the next host is asked about on its own terms.
            PermissionPromptAction::Deny => false,
            // "Not Now" also stops at this host for the session: it must not
            // dismiss the whole Network group, or every other URL would be
            // silently refused without a prompt.
            PermissionPromptAction::NotNow => {
                with_a2app(|state| {
                    for host in &network_hosts {
                        state.dismissed_net_hosts.insert((
                            prompt.subject.clone(),
                            host.trim().trim_end_matches('.').to_ascii_lowercase(),
                        ));
                    }
                });
                false
            }
            PermissionPromptAction::None => {
                with_a2app(|state| state.active_prompt = Some(prompt));
                return;
            }
        }
    } else if prompt.perm == Permission::McpTools {
        // An mcp-tools prompt grants ONE tool, not the whole group. A durable
        // group Deny would kill every tool this app/AI might use, which is a
        // broader decision than this prompt asks for, so an Allow records the
        // specific tool (with the description's content hash for a
        // registration) and a refusal is session-scoped.
        match (answer, prompt.tool.clone()) {
            (PermissionPromptAction::Allow, Some(tool)) => {
                with_a2app(|state| {
                    state.permissions.allow_tool(
                        &prompt.subject,
                        &tool.full_name,
                        &tool.content_hash,
                    );
                    // Keep the group "on" for App Info; it is a kill switch
                    // now, not the per-tool gate.
                    state.permissions.set(
                        &prompt.subject,
                        Permission::McpTools,
                        GrantState::Granted,
                    );
                    state.perms_dirty = true;
                });
                true
            }
            (PermissionPromptAction::AllowOnce, Some(tool)) => {
                with_a2app(|state| {
                    state.permissions.allow_tool_once(
                        &prompt.subject,
                        &tool.full_name,
                        &tool.content_hash,
                    )
                });
                true
            }
            (PermissionPromptAction::Deny | PermissionPromptAction::NotNow, Some(tool)) => {
                with_a2app(|state| state.permissions.deny_tool(&prompt.subject, &tool.full_name));
                false
            }
            (PermissionPromptAction::None, _) => {
                with_a2app(|state| state.active_prompt = Some(prompt));
                return;
            }
            // No tool attached (shouldn't happen): refuse rather than grant a
            // whole group on an ambiguous answer.
            (_, None) => false,
        }
    } else {
        match answer {
            PermissionPromptAction::Allow => {
                if !per_room {
                    with_a2app(|state| {
                        state.permissions.set(&prompt.subject, prompt.perm, GrantState::Granted);
                        state.perms_dirty = true;
                    });
                }
                true
            }
            PermissionPromptAction::AllowOnce => {
                // Session-only: never touches disk, dropped on isolate/session
                // teardown.
                if !per_room {
                    with_a2app(|state| state.permissions.grant_once(&prompt.subject, prompt.perm));
                }
                true
            }
            PermissionPromptAction::Deny => {
                with_a2app(|state| {
                    state.permissions.set(&prompt.subject, prompt.perm, GrantState::Denied);
                    state.perms_dirty = true;
                });
                false
            }
            PermissionPromptAction::NotNow => {
                with_a2app(|state| {
                    state.dismissed_prompts.insert((prompt.subject.clone(), prompt.perm));
                });
                false
            }
            PermissionPromptAction::None => {
                // Shouldn't happen; put the prompt back.
                with_a2app(|state| state.active_prompt = Some(prompt));
                return;
            }
        }
    };
    publish_grants(cx);

    let subject = prompt.subject.clone();
    let perm = prompt.perm;
    // Replay or refuse everything parked behind this prompt.
    for parked in prompt.parked {
        match parked {
            ParkedRequest::Bridge(request) => {
                if granted {
                    if let Some(request) = request {
                        let asks = with_a2app(|state| {
                            let A2AppState { broker, registry, permissions, foreground_app, .. } = state;
                            let is_docked = |app_id: &str| instances::is_docked(app_id);
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
                                room_name: &room_name,
                                desktop_view,
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
                    // Per-room grants for cross-room posts and reads: an Allow
                    // unlocks exactly the target room (durably; an Allow Once
                    // for this session only), recorded before re-running the
                    // job so its gate now lets it through.
                    let durable = matches!(answer, PermissionPromptAction::Allow);
                    let target = match &job {
                        SessionJob::PostRoomMessage { room_id: target, .. } => Some((target.clone(), true)),
                        SessionJob::ReadTool { kind: ReadToolKind::OtherRoom { room, .. }, .. } => Some((room.clone(), false)),
                        _ => None,
                    };
                    if let Some((target, is_post)) = target {
                        with_a2app(|state| {
                            if durable && is_post {
                                state.permissions.allow_room_send(&subject, &target);
                                state.perms_dirty = true;
                            } else if durable {
                                state.permissions.allow_room_read(&subject, &target);
                                state.perms_dirty = true;
                            } else {
                                state.once_rooms.insert((subject.clone(), perm, target));
                            }
                        });
                    }
                    // The grant is stored above; re-run the session job so its
                    // own gate now lets it through to the real work.
                    execute_session_job(cx, ui, &room_id, job);
                } else {
                    // Denied: the call is refused with a receipt naming it, so
                    // the turn's card shows the AI was stopped from doing it
                    // (and its live tool row is rewritten Done). Cross-room
                    // posts and reads are denied PER ROOM: reset the group to
                    // Ask so one room's refusal does not silently block every
                    // other room the agent might name later.
                    if matches!(
                        job,
                        SessionJob::PostRoomMessage { .. }
                            | SessionJob::ReadTool { kind: ReadToolKind::OtherRoom { .. }, .. }
                    ) {
                        with_a2app(|state| {
                            state.permissions.set(&subject, perm, GrantState::Ask);
                            state.perms_dirty = true;
                        });
                    }
                    let reason = ai_tool_refused_text(perm);
                    note_ai_tool_call(&room_id, &ai_job_tool_name(&job), false, &reason);
                    answer_session_job(job, Err(reason));
                }
            }
            #[cfg(unix)]
            ParkedRequest::NetworkAccess { host, answer, .. } => {
                let _ = if granted {
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

    if !is_agent_subject(&subject) {
        apply_permission_to_running(cx, ui, &subject, perm);
    }
    show_next_permission_prompt(cx, ui);
    ui.redraw(cx);
}

/// Pushes a changed grant into the app's live isolate: network changes
/// stop the app (the net runtime is baked in at VM alloc); anything else
/// just gets the new caps list plus an `on_permissions_changed` call.
fn apply_permission_to_running(cx: &mut Cx, ui: &WidgetRef, app_id: &str, perm: Permission) {
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
    if perm == Permission::Network {
        let Some(Some(manifest)) = with_a2app(|state| state.registry.get(app_id).cloned()) else { return };
        if stop_for_restart(cx, ui, &manifest) {
            enqueue_popup_notification(
                format!("\"{}\" was stopped; open it again with its new network access.", manifest.name),
                PopupKind::Info, Some(5.0),
            );
        }
    } else {
        instances::update_app_caps(cx, app_id, grants);
    }
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
    let withdrawn = with_a2app(|state| {
        let names: Vec<String> = state
            .app_tools
            .iter()
            .filter(|(_, reg)| reg.app_id == app_id)
            .map(|(name, _)| name.clone())
            .collect();
        for name in &names {
            if let Some(reg) = state.app_tools.remove(name)
                && let Some(session) = state.ai_sessions.get(&reg.room_id)
            {
                session.unregister_miniapp_tool(name);
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

/// Republishes the grant snapshot that isolate-creation sites read.
fn publish_grants(_cx: &mut Cx) {
    with_a2app(|state| {
        a2app_core::permissions::publish_snapshot(state.permissions.snapshot(&state.registry));
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
        if let Err(e) = persistence::save_permissions(&state.permissions) {
            error!("Failed to save mini-app permissions: {e}");
        }
        state.perms_dirty = false;
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
            had |= session.abort();
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
        AiRoomAction::PostReplyResult { room_id, answer_id, result } => {
            let parked = answer_id
                .and_then(|id| with_a2app(|state| state.ai_replies.remove(&id)).flatten());
            if let Err(error) = &result {
                log!("AI Rooms: FAILED to post an ai_reply state event: {error}");
                enqueue_popup_notification(
                    format!("Couldn't post the AI agent's reply: {error}"),
                    PopupKind::Error, Some(6.0),
                );
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
        AiRoomAction::ToolReadResult { id, result } => {
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
        AiRoomAction::StateEventPosted { room_id, event_type, success } => {
            // Release the coalescing guard for this room's turn card so the
            // next pending snapshot can be written (see
            // `flush_pending_ai_turns`), and adapt the write spacing: a rate
            // limit widens it, a success narrows it back. Other event types
            // share this feedback but gate nothing.
            if event_type.as_str() == AI_TURN_EVENT_TYPE {
                with_a2app(|state| {
                    if let Some(info) = state.ai_rooms.get_mut(&room_id) {
                        info.ai_turn_in_flight = false;
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
        }
    }
}

/// Stops an AI room's session and resets what a restart needs (one-time
/// grants, in-flight reads). Grants and restrictions — the user's durable
/// answers — are kept. Mirrors what the death path does, minus the popups;
/// used by the panel's power switch.
#[cfg(unix)]
fn stop_ai_session(room_id: &OwnedRoomId) {
    with_a2app(|state| {
        state.ai_sessions.remove(room_id);
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
            info.posted_by_tool_this_turn = false;
            info.pending_tool_calls.clear();
        }
    });
    close_active_turn(room_id);
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
        stop_ai_session(room_id);
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
            .effective_for(&subject, ai_room_declares_perm, perm)
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
            // Back to Ask: the next use prompts again (the prompt modal's
            // Allow/Deny answers then write over this).
            with_a2app(|state| {
                state.permissions.set(&subject, perm, GrantState::Ask);
                state.perms_dirty = true;
            });
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

/// Starts an AI room's session if it isn't already running. Started idle
/// (no prompt sent yet) — the first forwarded message primes it with the
/// replayed transcript and sends it as one prompt.
#[cfg(unix)]
fn attach_ai_session(cx: &mut Cx, ui: &WidgetRef, room_id: &OwnedRoomId, _name: Option<String>) {
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
        let started = AiSession::start(room_id.clone(), prefs).map(|session| {
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
        // Re-install app tools this room already had. Their descriptions were
        // reviewed when first registered, so a session restart does not ask
        // again; a tool whose isolate is gone is pruned by `sweep_app_tools`.
        with_a2app(|state| {
            let Some(session) = state.ai_sessions.get(room_id) else { return };
            let tools: Vec<(String, String, serde_json::Value)> = state
                .app_tools
                .values()
                .filter(|reg| &reg.room_id == room_id)
                .map(|reg| (reg.full_name.clone(), reg.description.clone(), reg.schema.clone()))
                .collect();
            for (name, description, schema) in tools {
                let _ = session.register_miniapp_tool(name, description, schema);
            }
        });
    }
    ui.redraw(cx);
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
            queued: session.queued_len(),
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
        state.ai_sessions.get(room_id).map(|s| s.is_busy() || s.queued_len() > 0)
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

/// Aborts what an AI room's agent is currently doing, in the order that lets
/// every piece settle: the app build its `launch_splash_app` tool call is
/// waiting on is cancelled first (dropping the [`Generation`] kills its agent
/// and resolves the waiting tool call, so the room's own session cannot stay
/// parked on it), then the session itself is asked to abandon its turn and
/// drop queued prompts. The session survives, idle, for the next message.
/// Only the room's own generation is touched — a build the Mini Apps screen
/// (or another room) started keeps running.
#[cfg(unix)]
fn abort_ai_room_work(cx: &mut Cx, ui: &WidgetRef, room_id: &OwnedRoomId) {
    // Cancel the room's own generation first, if one is running. Exactly the
    // Mini Apps screen's Stop, but scoped to this room's build.
    let cancels_generation = with_a2app(|state| {
        state.ai_generation_room.as_ref().is_some_and(|r| r == room_id)
            && state.generation.is_some()
    })
    .unwrap_or(false);
    if cancels_generation {
        with_a2app(|state| {
            // Dropping the Generation kills its agent child process.
            state.generation = None;
            state.console.status = String::from("Cancelled.");
        });
        resolve_session_generation(
            cx,
            ui,
            None,
            Err(String::from("The generation was cancelled.")),
        );
    }
    // Then abandon the session's turn (and drop anything queued behind it).
    // A no-op when the session is idle or already gone.
    with_a2app(|state| {
        if let Some(session) = state.ai_sessions.get_mut(room_id) {
            session.abort();
        }
    });
    // A prompt parked by the cancelled turn would run its tool call (post,
    // build, launch) whenever it was answered; withdraw it.
    refuse_room_prompts(cx, ui, room_id);
    // Settle the turn card immediately: a cancelled turn emits no `Reply`, and
    // if the turn is blocked on a tool/permission it may take a moment to end,
    // so the room's card would otherwise sit "running" after the user aborts.
    close_active_turn(room_id);
    ui.redraw(cx);
}

/// Forwards newly-seen member messages (in timeline order) to an AI room's
/// session as prompts, attaching the session first if needed. Nothing is sent
/// here beyond the member messages themselves: a session starts fresh and
/// silent, and its first (and only) inputs are real user messages.
///
/// Each successfully-forwarded event becomes the room's new cursor,
/// persisted as room account data for restart continuity.
#[cfg(unix)]
pub fn forward_ai_room_texts(
    room_id: &OwnedRoomId,
    new_texts: Vec<(OwnedEventId, String)>,
) {
    if new_texts.is_empty() {
        return;
    }
    log!("AI Rooms: forwarding {} new message(s) to room {room_id}'s session...", new_texts.len());
    for (event_id, text) in &new_texts {
        log!("AI Rooms:   -> forwarding {event_id}: {}", text.chars().take(120).collect::<String>());
    }
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
    let prefs = with_a2app(|state| state.agent_prefs.clone())
        .unwrap_or_else(a2app_agent::prefs::load_agent_prefs);
    let mut last_cursor = None;
    for (event_id, text) in new_texts {
        let outcome: Result<PromptOutcome, String> = with_a2app(|state| {
            if !state.ai_sessions.contains_key(room_id) {
                let started = AiSession::start(room_id.clone(), prefs.clone());
                if let Some(info) = state.ai_rooms.get_mut(room_id) {
                    info.start_failed_at = started.is_err().then(Instant::now);
                }
                state.ai_sessions.insert(room_id.clone(), started?);
            }
            let outcome = state.ai_sessions.get_mut(room_id)
                .map(|session| session.prompt(text.clone()))
                .unwrap_or(PromptOutcome::Dead);
            if outcome != PromptOutcome::Dead {
                if let Some(info) = state.ai_rooms.get_mut(room_id) {
                    info.cursor = Some(event_id.clone());
                }
            }
            Ok(outcome)
        }).unwrap_or(Ok(PromptOutcome::Dead));
        match outcome {
            Ok(PromptOutcome::Dead) => {
                log!("AI Rooms: room {room_id}'s agent session is dead; NOT forwarding {event_id}.");
                enqueue_popup_notification(
                    "This AI room's agent has stopped; it restarts on your next message.",
                    PopupKind::Warning, Some(6.0),
                );
                return;
            }
            Err(e) => {
                log!("AI Rooms: FAILED to start/use room {room_id}'s agent session: {e}");
                enqueue_popup_notification(
                    format!("Couldn't start this AI room's agent: {e}"),
                    PopupKind::Error, Some(6.0),
                );
                return;
            }
            Ok(other) => log!("AI Rooms: forwarded {event_id} to room {room_id}'s session: {other:?}"),
        }
        last_cursor = Some(event_id);
    }
    if let Some(cursor) = last_cursor {
        submit_async_request(MatrixRequest::AiRoom(AiRoomRequest::SaveCursor {
            room_id: room_id.clone(),
            cursor,
        }));
    }
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
            .filter(|(_, reg)| instances::key_of_heap(reg.heap_key).is_none())
            .map(|(name, _)| name.clone())
            .collect()
    })
    .unwrap_or_default();
    for name in dead {
        with_a2app(|state| {
            if let Some(reg) = state.app_tools.remove(&name) {
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
    state.ai_reads.values().any(|(r, _, _)| r == room_id)
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
        || state.prompts.iter().any(|p| prompt_parks_room_tool(p, room_id))
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
                        let summary: String = summary.chars().take(200).collect();
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
                        } else {
                            to_post.push((room_id.clone(), text))
                        }
                        // Turn over: the turn card is settled (Done), and
                        // any tool call that never resolved stays as a
                        // `Started` line inside it — the reply's receipt chips
                        // still tell the outcome.
                        close_active_turn(&room_id);
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
                        close_active_turn(&room_id);
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
                        close_active_turn(&room_id);
                        log!("AI Rooms: room {room_id}'s agent session reported an error: {msg}");
                        post_ai_activity(&room_id, AiActivityKind::Error, Some(&msg));
                        errors.push(msg)
                    }
                    SessionUpdate::Gone(msg) => {
                        log!("AI Rooms: room {room_id}'s agent session is GONE: {msg}");
                        close_active_turn(&room_id);
                        post_ai_activity(&room_id, AiActivityKind::Stopped, Some(&msg));
                        deaths.push((room_id.clone(), msg))
                    }
                }
            }
        }
    }
    for (room_id, text) in to_post {
        post_ai_reply(&room_id, text, None);
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
        // as the panel's power-off does it.
        refuse_room_prompts(cx, ui, &room_id);
        stop_ai_session(&room_id);
        ui.redraw(cx);
    }

    // 3. Keep each room's busy/queued status row honest between timeline
    //    updates: a turn ending with no reply, the queue flushing the next
    //    prompt, or the agent finishing its startup all change it without a
    //    new timeline event. Redraw rooms whose row would change, once per
    //    pass (the row's text itself is populated at draw time).
    let status_changed: Vec<OwnedRoomId> = with_a2app(|state| {
        let mut changed = Vec::new();
        for (room_id, session) in state.ai_sessions.iter() {
            let (busy, queued) = (session.is_busy(), session.queued_len());
            let Some(info) = state.ai_rooms.get_mut(room_id) else { continue };
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

    // 4. Emit at most one coalesced turn snapshot per room, spaced under the
    //    homeserver's state-event rate limit. Any room whose write is still
    //    in flight (or cooling down) keeps the flush timer armed.
    flush_pending_ai_turns(cx);
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
    state.permissions.effective_capability_for(&subject, ai_room_declares_perm, ai_room_declares_cap, cap)
}

/// Executes one capability-gated attached-room read for a session. Decides
/// against the room's permission subject (`agent_subject(room_id)`) exactly as
/// the broker decides for a mini-app: a granted call records its use and is
/// fetched on the async worker (the tool call waits, answered when the result
/// lands); a refused call answers with an error the model can read and act on;
/// a first use parks the call behind the permission prompt until the user
/// answers.
#[cfg(unix)]
fn run_ai_read_tool(
    cx: &mut Cx,
    ui: &WidgetRef,
    room_id: &OwnedRoomId,
    kind: ReadToolKind,
    answer: Sender<Result<String, String>>,
) {
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
        ReadToolKind::OtherRoom { room, .. } => match OwnedRoomId::try_from(room.as_str()) {
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
    let verdict = match &target_room {
        Some(target) => with_a2app(|state| {
            let store = &state.permissions;
            let group_state = cap.group.map(|g| store.state(&subject, g));
            let once = cap
                .group
                .is_some_and(|g| state.once_rooms.contains(&(subject.clone(), g, target.to_string())));
            if group_state == Some(GrantState::Denied) {
                Effective::Denied
            } else if group_state == Some(GrantState::Granted) {
                // The AI panel's explicit "allow all rooms" grant.
                Effective::Granted
            } else if once || store.is_room_read_allowed(&subject, target.as_str()) {
                Effective::Granted
            } else {
                Effective::NeedsPrompt
            }
        })
        .unwrap_or(Effective::Denied),
        None => {
            with_a2app(|state| ai_capability_verdict(state, room_id, cap)).unwrap_or(Effective::Denied)
        }
    };
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
            submit_async_request(MatrixRequest::AiRoom(AiRoomRequest::ToolRead {
                id,
                room_id: room_id.clone(),
                tool: kind,
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
    let (verdict, write_off) = with_a2app(|state| {
        let store = &state.permissions;
        let verdict = if store.state(&subject, group) == GrantState::Denied {
            Some(false)
        } else if store.is_room_send_allowed(&subject, &target)
            || state.once_rooms.contains(&(subject.clone(), group, target.to_string()))
        {
            Some(true)
        } else {
            None
        };
        (verdict, !store.matrix_write())
    })
    .unwrap_or((Some(false), false));
    let job = SessionJob::PostRoomMessage {
        room_id: target,
        text,
        answer,
    };
    // The global write switch covers the agent's cross-room posts like any
    // other switch-gated write.
    if write_off {
        let text = format!("The AI can't post: {MATRIX_WRITE_OFF_MSG}.");
        note_ai_tool_call(session_room, "post_room_message", false, &text);
        answer_session_job(job, Err(text));
        return;
    }
    match verdict {
        Some(true) => {
            // Allowed for this room: record the group use and post on the
            // async worker (the tool call waits, answered when the write
            // lands).
            with_a2app(|state| {
                state.permissions.record_access(&subject, group, versions::now_unix());
                state.perms_dirty = true;
            });
            post_ai_room_message(session_room, &target_room, job);
        }
        Some(false) => {
            let text = ai_tool_refused_text(group);
            note_ai_tool_call(session_room, "post_room_message", false, &text);
            answer_session_job(job, Err(text));
        }
        None => {
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
/// launch — account-scoped apps plus apps scoped to this room — as JSON the
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
                    .filter(|m| match &m.scope {
                        A2AppScope::Account => true,
                        A2AppScope::Room { room_id: owner } => owner == room_id.as_str(),
                    })
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
            let text = serde_json::json!({ "apps": apps }).to_string();
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
/// scoped to another room, or restricted is an error the model reads and can
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
        OtherRoom(MiniAppId),
        Restricted(MiniAppId),
    }
    let outcome: Result<MiniAppManifest, Refusal> = with_a2app(|state| {
        let Some(manifest) = state.registry.get(&app_id).cloned() else {
            return Err(Refusal::Unknown);
        };
        match &manifest.scope {
            A2AppScope::Account => {}
            A2AppScope::Room { room_id: owner } if owner == room_id.as_str() => {}
            A2AppScope::Room { .. } => return Err(Refusal::OtherRoom(manifest.id)),
        }
        if state.permissions.is_restricted(&manifest.id) {
            return Err(Refusal::Restricted(manifest.id));
        }
        Ok(manifest)
    })
    .unwrap_or(Err(Refusal::Unknown));

    match outcome {
        Ok(manifest) => {
            // Dock it into this room exactly as a freshly built app is docked
            // (`resolve_session_generation`), so "launch" lands where the
            // conversation is.
            cx.action(A2AppOp::OpenApp {
                app_id: manifest.id.clone(),
                room_id: Some(room_id.clone()),
                in_room_pane: true,
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
                Refusal::OtherRoom(id) => format!(
                    "\"{id}\" belongs to another room, so it can't run here. Use list_apps for the apps available in this room."
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

/// Runs one tool call the room's agent made, on the UI thread, and sends the
/// result back to the serve thread that called the tool.
#[cfg(unix)]
fn execute_session_job(cx: &mut Cx, ui: &WidgetRef, room_id: &OwnedRoomId, job: SessionJob) {
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
    let resolved_tool: Option<String> =
        if with_a2app(|state| state.app_tools.contains_key(&tool)).unwrap_or(false) {
            Some(tool.clone())
        } else {
            with_a2app(|state| {
                let mut matches = state
                    .app_tools
                    .values()
                    .filter(|reg| &reg.room_id == room_id && reg.raw_name == tool)
                    .map(|reg| reg.full_name.clone());
                let first = matches.next()?;
                if matches.next().is_none() { Some(first) } else { None }
            })
            .flatten()
        };
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
        state.permissions.tool_effective(&subject, &tool, None)
    })
    .unwrap_or(Effective::NeedsPrompt);
    match decided {
        Effective::Granted => {
            run_app_tool_invocation(cx, room_id, tool, arguments, display_name, answer);
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
    let call_id = NEXT_APP_TOOL_CALL_ID.fetch_add(1, Ordering::Relaxed);
    with_a2app(|state| {
        state.app_tool_calls.insert(
            call_id,
            AppToolPending {
                room_id: room_id.clone(),
                app_id,
                full_name: tool.clone(),
                display_name,
                answer,
                since: Instant::now(),
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
            let _ = pending.answer.send(Err(format!(
                "the mini-app did not handle the tool call `{tool}`"
            )));
        }
    }
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
    let subject = agent_subject(room_id.as_str());
    let verdict = with_a2app(|state| {
        if state.permissions.is_restricted(&subject) {
            return NetworkVerdict::Denied;
        }
        // A durable group Denied is the kill switch for all internet access.
        if state.permissions.state(&subject, Permission::Network) == GrantState::Denied {
            return NetworkVerdict::Denied;
        }
        if state.permissions.is_host_allowed(&subject, host) {
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
            cx.action(A2AppOp::OpenApp {
                app_id: manifest.id.clone(),
                room_id: Some(room_id),
                in_room_pane: true,
            });
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
    let summary: String = summary.chars().take(48).collect();
    // `finish_ai_tool_call` returns the target detail recorded for the call
    // when its job reached the UI thread, so the receipt chip names the same
    // room/space/app the live row did.
    let detail = finish_ai_tool_call(room_id, name, ok, &summary);
    push_ai_tool_receipt(room_id, name, detail, ok, &summary);
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
                summary: summary.chars().take(48).collect(),
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
        call.summary = summary.chars().take(48).collect();
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
    post_ai_state_event(room_id, AI_ACTIVITY_EVENT_TYPE, &next_ai_state_key("activity"), &content);
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

/// Settles and clears this room's open turn: rewrites its `ai_turn` row
/// `Done` (turning the card's tint neutral and hiding its spinner) and drops
/// it, so the next turn gets a fresh card. A no-op when no turn is open.
#[cfg(unix)]
fn close_active_turn(room_id: &OwnedRoomId) {
    let closed = with_a2app(|state| {
        let info = state.ai_rooms.get_mut(room_id)?;
        let mut turn = info.active_turn.take()?;
        // The final snapshot gets the highest `seq` of the turn, so it wins the
        // renderer's newest-snapshot selection even if it lands before an
        // earlier rewrite.
        turn.seq = turn.seq.saturating_add(1);
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
    if let Some((key, content)) = closed {
        post_ai_turn(room_id, &key, &content);
    }
}

/// The starting minimum spacing between `ai_turn` state-event writes for one
/// room. matrix.org responds `429 M_LIMIT_EXCEEDED` to state events written
/// faster than roughly this (`retry_after` was observed at 4s), and a busy
/// turn can otherwise produce dozens of snapshots.
#[cfg(unix)]
const AI_TURN_POST_MIN_INTERVAL: Duration = Duration::from_secs(5);
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
    let mut to_post: Vec<(OwnedRoomId, String, AiTurnContent)> = Vec::new();
    let mut needs_timer = false;
    with_a2app(|state| {
        for (room_id, info) in state.ai_rooms.iter_mut() {
            if info.ai_turn_in_flight {
                needs_timer = true;
                continue;
            }
            if info.pending_ai_turns.is_empty() {
                continue;
            }
            let cooled_down = info
                .last_ai_turn_post
                .is_none_or(|last| now.duration_since(last) >= info.ai_turn_backoff);
            if !cooled_down {
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
                info.ai_turn_in_flight = true;
                info.last_ai_turn_post = Some(now);
                to_post.push((room_id.clone(), key, content));
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
    for (room_id, key, content) in to_post {
        post_ai_state_event(&room_id, AI_TURN_EVENT_TYPE, &key, &content);
    }
}

/// Serializes one AI-session activity/tool-call row and hands it to the
/// async worker as a [`AiRoomRequest::PostAiStateEvent`]. Best-effort: the
/// worker logs failures and the turn continues (the reply's receipts are the
/// durable record).
#[cfg(unix)]
fn post_ai_state_event(
    room_id: &OwnedRoomId,
    event_type: &str,
    state_key: &str,
    content: &impl serde::Serialize,
) {
    match serde_json::to_value(content) {
        Ok(json) => {
            submit_async_request(MatrixRequest::AiRoom(AiRoomRequest::PostAiStateEvent {
                room_id: room_id.clone(),
                event_type: event_type.to_string(),
                state_key: state_key.to_string(),
                content: json,
            }));
        }
        Err(e) => log!("AI Rooms: couldn't serialize an {event_type} state row: {e}"),
    }
}

/// Writes `text` as an `ai_reply` state event — the agent's only output
/// channel (never `m.room.message`, so it can't loop back into the
/// forwarder as input). This is how both a completed turn's natural reply
/// and the `send_message` tool call reach the room. The turn's pending tool
/// receipts are consumed onto this one card.
#[cfg(unix)]
fn post_ai_reply(room_id: &OwnedRoomId, text: String, answer_id: Option<u64>) {
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
    submit_async_request(MatrixRequest::AiRoom(AiRoomRequest::PostReply {
        room_id: room_id.clone(),
        answer_id,
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
}
