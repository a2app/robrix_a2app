//! The host-service broker: the host half of the `host.request(...)` bridge.
//! Drains queued requests from every Splash isolate, applies the permission
//! policy, does the platform work through the robius crates, and answers back
//! into the requesting isolate.
//!
//! Split of responsibilities with the host app: the broker decides and
//! executes everything it can locally; anything that must touch host state or
//! widgets (showing a prompt, delivering IPC into other isolates, popup
//! notifications, the matrix services) is returned as a [`BrokerAsk`] for the
//! host's event-handling code to perform. Robius callbacks land on other
//! threads, so they come home through an mpsc channel drained on the next
//! event pass (`SignalToUI` wakes one promptly).

pub mod limits;
pub mod matrix;

pub use matrix::{MatrixServiceCall, SearchScope};

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;

use makepad_widgets::splash_host::{
    splash_host_respond, take_splash_host_requests, SplashHostRequest,
};
use makepad_widgets::*;

use crate::layout::PaneSide;
use crate::manifest::{AppRegistry, MiniAppId};
use crate::permissions::{Effective, Permission, PermissionContext, PermissionStore, PolicyDecision, RoomAccess};

pub const PLATFORM: &str = if cfg!(target_os = "macos") {
    "macos"
} else if cfg!(target_os = "ios") {
    "ios"
} else if cfg!(target_os = "android") {
    "android"
} else if cfg!(target_os = "windows") {
    "windows"
} else if cfg!(target_os = "linux") {
    "linux"
} else {
    "other"
};

/// The IP-geolocation fallback endpoint (city-level, no key needed).
const GEO_URL: &str = "https://ipapi.co/json/";

/// Where a service answer must go: one isolate, one request.
#[derive(Clone, Copy, Debug)]
pub struct Reply {
    pub heap_key: usize,
    pub req_id: u64,
}

/// A host-observed failure, attributed to the exact requesting isolate.
pub struct ServiceFailure {
    pub app_id: MiniAppId,
    pub heap_key: usize,
    pub error: String,
    /// Repeated identical errors still update task status, without popup spam.
    pub show_popup: bool,
}

impl Reply {
    fn of(req: &SplashHostRequest) -> Reply {
        Reply { heap_key: req.heap_key, req_id: req.req_id }
    }
}

/// Whether a dispatch spends from the app's request budget. Fresh requests
/// do; a request replayed after the user answered a permission prompt does
/// not, since it was already paid for when the app first asked.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Charge {
    Yes,
    No,
}

/// Appends one line to the service trace (`ROBRIX_A2APP_TRACE_SERVICES=1`).
/// A file, not stdout: this has to work inside a test harness that swallows
/// the app's console output.
fn trace_line(line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/robrix_a2app_services.log")
    {
        let _ = f.write_all(line.as_bytes());
    }
}

fn trace_on() -> bool {
    std::env::var("ROBRIX_A2APP_TRACE_SERVICES").is_ok()
}

thread_local! {
    /// Every answer given this pass, so the host can tell the user about a
    /// failed action after the fact (see [`Broker::failed_acts`]).
    static ANSWERS: RefCell<Vec<(Reply, Option<String>)>> = RefCell::new(Vec::new());
}

/// Answers one request. `Ok` carries the `data` JSON, `Err` the user-visible
/// error string.
pub fn respond(cx: &mut Cx, reply: Reply, result: Result<&str, &str>) {
    ANSWERS.with(|a| a.borrow_mut().push((reply, result.err().map(str::to_string))));
    let outcome = splash_host_respond(cx, reply.heap_key, reply.req_id, result);
    if trace_on() {
        trace_line(&format!(
            "respond heap={} req={} ok={} outcome={:?}\n",
            reply.heap_key,
            reply.req_id,
            result.is_ok(),
            outcome,
        ));
    }
}

/// How many tools one instance may have registered with the room's AI at
/// once. Bounds what a runaway script can push into the model's context.
pub const MAX_APP_TOOLS_PER_INSTANCE: usize = 16;
/// Cap on an app-authored tool description, in chars. It is shown to the user
/// AND handed to the model, so it is bounded before either.
pub const MAX_APP_TOOL_DESCRIPTION_CHARS: usize = 1200;
/// Cap on an app-authored tool result, in chars: it goes straight into the
/// model's context, so it is bounded like the description.
pub const MAX_APP_TOOL_RESULT_CHARS: usize = 32_000;
/// Cap on the app-authored tool name, in chars.
pub const MAX_APP_TOOL_NAME_CHARS: usize = 48;
/// Cap on the number of arguments a tool may declare.
pub const MAX_APP_TOOL_ARGS: usize = 20;

/// A parsed `mcp.tools.register`: everything the user must review and the
/// runtime needs to install the tool on a room's agent session.
#[derive(Clone, Debug)]
pub struct AppToolRequest {
    pub app_id: MiniAppId,
    /// The instance's host tag (app plus room), which scopes the tool.
    pub instance_tag: String,
    /// The attached room, when the instance has one. The tool is registered
    /// on THAT room's AI session.
    pub room: Option<String>,
    /// The registering isolate, so invocations route back to exactly it.
    pub heap_key: usize,
    /// The app's chosen name, as written.
    pub name: String,
    /// The namespaced name the model will see (`app_<id>_<name>`).
    pub full_name: String,
    /// The app-authored description, verbatim.
    pub description: String,
    /// The JSON Schema built from the app's flat `args`.
    pub schema: serde_json::Value,
    /// `(name, type, description)` per argument, for the consent prompt.
    pub args: Vec<(String, String, String)>,
    /// Hash of (description, args): a changed value must be re-reviewed.
    pub content_hash: String,
}

/// The namespaced MCP name for a tool an app registered: `app_<id>_<name>`,
/// with anything outside `[a-z0-9_]` folded to `_`. Namespacing is what keeps
/// an app from shadowing a built-in tool or another app's tool.
pub fn app_tool_full_name(app_id: &str, name: &str) -> String {
    fn clean(s: &str) -> String {
        s.chars()
            .map(|c| {
                let c = c.to_ascii_lowercase();
                if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }
            })
            .collect()
    }
    format!("app_{}_{}", clean(app_id), clean(name))
}

/// A stable content hash for a tool registration, over the description and
/// the argument list in order. Stored with the grant so a changed description
/// no longer matches and the user is asked again.
fn app_tool_content_hash(description: &str, args: &[(String, String, String)]) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    description.hash(&mut hasher);
    for (name, ty, desc) in args {
        name.hash(&mut hasher);
        ty.hash(&mut hasher);
        desc.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

/// The text an `mcp.tools.result` hands the model, clamped to
/// [`MAX_APP_TOOL_RESULT_CHARS`] with a marker so the model knows it was cut.
fn app_tool_result_text(result: &serde_json::Value) -> String {
    let mut text = match result {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    };
    if text.chars().count() > MAX_APP_TOOL_RESULT_CHARS {
        text = text.chars().take(MAX_APP_TOOL_RESULT_CHARS).collect();
        text.push_str("… [truncated]");
    }
    text
}

/// Validates and normalizes a `mcp.tools.register` request into an
/// [`AppToolRequest`]. Every bound here is a bound on text that can reach the
/// model, so refusals are explicit and early. `mcp.tools.*` bypass the
/// generic group gate, so the declaration check lives here instead.
pub fn parse_app_tool_request(
    manifest: &crate::manifest::MiniAppManifest,
    req: &SplashHostRequest,
    args: &serde_json::Value,
) -> Result<AppToolRequest, String> {
    if !manifest.declares(Permission::McpTools) {
        return Err(format!("permission not declared: {}", Permission::McpTools.as_str()));
    }
    // A tool lands on the attached room's AI session; with no room there is
    // nothing to register it on, so don't prompt for a grant that can't work.
    let (_, room) = crate::manifest::split_instance_tag(&req.app_tag);
    if room.is_none() {
        return Err(String::from("this mini-app is not attached to a room"));
    }
    let name = args["name"]
        .as_str()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .ok_or("mcp.tools.register needs a {name}")?;
    if name.chars().count() > MAX_APP_TOOL_NAME_CHARS {
        return Err(format!("tool name is too long (max {MAX_APP_TOOL_NAME_CHARS} characters)"));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(String::from("tool name may only use letters, digits, '_' and '-'"));
    }
    let description = args["description"]
        .as_str()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .ok_or("mcp.tools.register needs a {description}")?;
    if description.chars().count() > MAX_APP_TOOL_DESCRIPTION_CHARS {
        return Err(format!(
            "tool description is too long (max {MAX_APP_TOOL_DESCRIPTION_CHARS} characters)"
        ));
    }
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    let mut parsed_args = Vec::new();
    if let Some(list) = args.get("args") {
        let Some(list) = list.as_array() else {
            return Err(String::from("`args` must be an array of {name, type, description} objects"));
        };
        if list.len() > MAX_APP_TOOL_ARGS {
            return Err(format!("too many arguments (max {MAX_APP_TOOL_ARGS})"));
        }
        for item in list {
            let arg_name = item["name"]
                .as_str()
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .ok_or("each argument needs a {name}")?;
            if !arg_name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(format!(
                    "argument name `{arg_name}` may only use letters, digits and '_'"
                ));
            }
            let ty = item["type"].as_str().unwrap_or("string");
            if !matches!(ty, "string" | "integer" | "number" | "boolean" | "array" | "object") {
                return Err(format!("argument `{arg_name}` has unknown type `{ty}`"));
            }
            let desc = item["description"].as_str().unwrap_or("");
            if properties.contains_key(arg_name) {
                return Err(format!("argument `{arg_name}` is declared twice"));
            }
            properties.insert(
                arg_name.to_string(),
                serde_json::json!({ "type": ty, "description": desc }),
            );
            required.push(serde_json::json!(arg_name));
            parsed_args.push((arg_name.to_string(), ty.to_string(), desc.to_string()));
        }
    }
    let schema = serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    });
    let full_name = app_tool_full_name(&manifest.id, name);
    let content_hash = app_tool_content_hash(description, &parsed_args);
    Ok(AppToolRequest {
        app_id: manifest.id.clone(),
        instance_tag: req.app_tag.clone(),
        room: room.map(str::to_string),
        heap_key: req.heap_key,
        name: name.to_string(),
        full_name,
        description: description.to_string(),
        schema,
        args: parsed_args,
        content_hash,
    })
}

/// Work only the host can do, returned from [`Broker::process`].
pub enum BrokerAsk {
    /// Finish only the run belonging to this requesting isolate/activation.
    BackgroundComplete { reply: Reply, run_id: u64, success: bool },
    /// HTTP is executed by the host after checking the current source labels.
    Network { reply: Reply, app_id: MiniAppId, room: Option<String>, args: serde_json::Value, consent: Box<PermissionStore> },
    /// Queue a runtime-permission prompt. `request`, when present, is parked
    /// until the user answers (re-dispatch on allow, deny-respond on deny).
    Prompt {
        app_id: MiniAppId,
        perm: Permission,
        request: Option<SplashHostRequest>,
        /// For an `mcp-tools` prompt: the exact tool under review, so the
        /// modal can show the app-authored description and the answer can
        /// record a per-tool grant.
        tool: Option<AppToolRequest>,
    },
    /// Deliver an (already policy-checked) IPC message to `to`'s running
    /// isolates, then answer `reply` with the delivered count. `from_heap`
    /// identifies the SENDING isolate so a `to: "self"` broadcast doesn't
    /// echo the message back to its own sender.
    IpcDeliver {
        reply: Reply,
        from: MiniAppId,
        from_heap: usize,
        to: MiniAppId,
        data_json: String,
        /// False for ipc.post: its fixed acknowledgement was already sent.
        receipt: bool,
    },
    /// Show a popup notification for this app. The request is already
    /// answered; `summary` is the popup text, clamped broker-side.
    Notify { app_id: MiniAppId, summary: String },
    /// An app actually USED a capability: record it for the access log and
    /// light the in-use indicator. Only real, policy-passed uses reach here.
    Used { app_id: MiniAppId, perm: Permission },
    /// This app has abused the bridge past the point of being refused
    /// politely: stop it, mark it restricted, and tell the user. The broker
    /// has already answered the offending request with an error.
    Restrict { app_id: MiniAppId, reason: String },
    /// Run a validated matrix.* call against the SDK, then answer `reply`
    /// with [`respond`] on the UI thread when the result comes back.
    Matrix {
        reply: Reply,
        app_id: MiniAppId,
        /// The room this instance is bound to (from its host tag).
        room: Option<String>,
        capability: &'static str,
        consent: Box<PermissionStore>,
        call: MatrixServiceCall,
    },
    /// Steer the host UI (navigate, or touch the composer), then answer
    /// `reply`. Ids are validated by the host, which knows the id types.
    HostAction {
        reply: Reply,
        app_id: MiniAppId,
        action: HostAction,
    },
    /// This isolate wants `hook` for its room (none for an account-wide
    /// hook); already permission-checked.
    Subscribe {
        reply: Reply,
        app_id: MiniAppId,
        heap_key: usize,
        room: Option<String>,
        hook: &'static str,
    },
    /// Drop one hook (or all with `None`) for this isolate.
    Unsubscribe {
        reply: Reply,
        heap_key: usize,
        hook: Option<&'static str>,
    },
    /// A fact only the host holds; answered with `reply` on the UI thread.
    HostQuery { reply: Reply, query: HostQuery },
    /// Install a reviewed mini-app tool on the room's agent session, then
    /// answer `reply` with the namespaced tool name the model will see.
    McpRegisterTool { reply: Reply, request: AppToolRequest },
    /// Withdraw one of an instance's tools from the room's session, then
    /// answer `reply`.
    McpUnregisterTool { reply: Reply, app_id: MiniAppId, heap_key: usize, full_name: String },
    /// An app answered one `on_tool_call`: hand the result to the serve thread
    /// parked on it, then answer `reply`.
    McpToolResult { reply: Reply, app_id: MiniAppId, call_id: u64, ok: bool, text: String },
}

pub enum HostQuery {
    Prefs,
    DeviceInfo,
}

/// Where the calling instance is on screen, for `env` and `ui.pane.read`.
#[derive(Clone, Debug)]
pub struct PaneState {
    /// `dock`, `tab` or `modal`; `parked` while nothing shows it.
    pub surface: &'static str,
    pub side: Option<PaneSide>,
    pub minimized: bool,
    pub foreground: bool,
    pub width: f64,
    pub height: f64,
}

impl PaneState {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "surface": self.surface,
            "side": self.side.map(PaneSide::as_str),
            "minimized": self.minimized,
            "foreground": self.foreground,
            "width": self.width,
            "height": self.height,
        })
    }
}

/// A parsed nav.* / composer.* call. `room` is the explicit `room_id`
/// argument, else the instance's own room; `None` means neither was given.
pub enum HostAction {
    OpenRoom { room: String },
    JumpToEvent { room: Option<String>, event_id: String },
    OpenThread { room: Option<String>, event_id: String },
    ShowUser { room: Option<String>, user_id: String },
    OpenSpace { space: String },
    /// One of `home`, `add_room`, `mini_apps`, `settings`.
    OpenScreen { screen: String },
    /// A `matrix.to` / `matrix:` link, resolved host-side.
    OpenLink { room: Option<String>, url: String },
    OpenApp { room: Option<String>, app_id: MiniAppId },
    ComposerInsert { room: Option<String>, text: String },
    ComposerReplyTo { room: Option<String>, event_id: String },
    /// The `ui.pane.*` calls, acting on the calling instance's own pane.
    ClosePane,
    SetSide { side: PaneSide },
    Minimize,
    BreakOut,
}



/// Async results coming home from robius callbacks on other threads.
enum Completion {
    Respond(Reply, Result<String, String>),
    /// CoreLocation produced a fix: answer every waiting location request.
    LocationFix { lat: f64, lon: f64 },
    /// CoreLocation failed (denied / unavailable): fall back to IP geolocation.
    LocationFailed,
}

/// The host state the broker reads while dispatching, borrowed fresh each
/// event pass.
#[derive(Clone, Copy)]
pub struct BrokerCtx<'a> {
    pub registry: &'a AppRegistry,
    pub permissions: &'a PermissionStore,
    /// The app whose host pane is currently shown, gating the UI services
    /// and the one-at-a-time modal guard.
    pub foreground_app: Option<&'a str>,
    /// Whether an app has a live instance docked on a room screen; those are
    /// on screen too, so UI-class services must not treat them as background.
    pub is_docked: &'a dyn Fn(&str) -> bool,
    pub is_running: &'a dyn Fn(&str) -> bool,
    /// The calling isolate's pane, by heap key.
    pub pane_state: &'a dyn Fn(usize) -> Option<PaneState>,
    /// Actual live compartment storage, never the legacy shared app directory.
    pub storage_path: &'a dyn Fn(usize) -> Result<std::path::PathBuf, String>,
    /// A room's display name, for `env`.
    pub room_name: &'a dyn Fn(&str) -> Option<String>,
    pub desktop_view: bool,
    /// Host-trusted provenance and output checks, before service side effects.
    pub check_flow: &'a dyn Fn(&SplashHostRequest, &crate::capabilities::Capability, &serde_json::Value, &AppRegistry) -> Result<(), String>,
    /// Revalidate the live context and record returned sources before callbacks.
    pub check_response: &'a dyn Fn(Reply, &str) -> Result<(), String>,
}

/// The refusal every switch-gated write gets while the user keeps writes off.
pub const MATRIX_WRITE_OFF_MSG: &str =
    "room access is blocked by the safety rules in Mini Apps";

/// The actual target of a bridge request. Attached-room services ignore an
/// unrecognized `room_id` argument, so their policy check must ignore it too.
pub fn permission_context<'a>(
    service: &str,
    args: &'a serde_json::Value,
    origin_room: Option<&'a str>,
) -> PermissionContext<'a> {
    let arg = |key: &str| args[key].as_str().map(str::trim).filter(|s| !s.is_empty());
    let target_room = match service {
        "matrix.rooms_info" | "matrix.rooms_messages" | "matrix.rooms_send"
        | "matrix.invite_respond" | "nav.room" => arg("room_id"),
        "matrix.space_info" | "matrix.space_rooms" | "nav.space" => arg("space_id"),
        "matrix.room_preview" | "matrix.join" => arg("room"),
        "nav.event" | "nav.thread" | "nav.user" | "nav.app"
        | "composer.insert" | "composer.reply_to" => arg("room_id").or(origin_room),
        _ => origin_room,
    };
    PermissionContext { origin_room, target_room }
}

/// Collection consent starts the request; individual returned rooms must
/// independently pass the ordinary target-specific gate.
pub fn is_room_collection(capability: &crate::capabilities::Capability) -> bool {
    matches!(capability.id,
        "matrix.rooms.list" | "matrix.rooms.search" | "matrix.rooms.invites.list"
        | "matrix.rooms.messages.search" | "matrix.spaces.list" | "matrix.space.rooms.list"
        | "on_rooms_changed" | "on_invite_received" | "on_unread_totals_changed"
    )
}

/// A group query describes whether this instance can already use a declared
/// capability. It does not create a group grant: every subsequent operation
/// still checks its own capability and actual room target.
fn permission_request_status(
    store: &PermissionStore,
    manifest: &crate::manifest::MiniAppManifest,
    permission: Permission,
    context: PermissionContext<'_>,
) -> Effective {
    let base = store.effective_for_in_context(&manifest.id, |p| manifest.declares(p), permission, context);
    if matches!(base, Effective::Denied | Effective::Undeclared) { return base; }
    let mut any = false;
    let mut all_denied = true;
    for capability in crate::capabilities::in_group(permission)
        .filter(|cap| cap.is_available() && manifest.declares_capability(cap))
    {
        any = true;
        let effective = if is_room_collection(capability) {
            store.effective_collection_capability_in_context(manifest, capability, context)
        } else {
            store.effective_capability_in_context(manifest, capability, context)
        };
        if effective == Effective::Granted { return Effective::Granted; }
        all_denied &= effective == Effective::Denied;
    }
    if any && all_denied { Effective::Denied } else { base }
}

pub struct Broker {
    tx: Sender<Completion>,
    rx: Receiver<Completion>,
    /// Kept alive so CoreLocation's delegate keeps reporting; created lazily
    /// on first use (must be on the main thread, which `process` is).
    location_manager: Option<robius_location::Manager>,
    /// Location requests waiting on CoreLocation or the IP fallback.
    pending_locations: Vec<Reply>,
    /// In-flight host-side IP-geolocation fetches, by request LiveId.
    pending_geo: HashMap<LiveId, Reply>,
    /// Per-app request budgets and strikes (`limits`): what keeps a hostile
    /// app from turning the bridge into a denial-of-service on the host.
    limits: limits::AbuseLimiter,
    /// Which app owns each on-screen OS dialog, so the one-at-a-time guard
    /// can be released when the completion comes home from another thread.
    dialog_owner: HashMap<(usize, u64), MiniAppId>,
    /// Requests whose error answer the user must see, not just the script:
    /// every act on their behalf, and every refusal of a request.
    notable: HashMap<(usize, u64), MiniAppId>,
    /// When each (app, message) was last shown, so a retry loop is one popup.
    shown: HashMap<(MiniAppId, String), std::time::Instant>,
}

impl Default for Broker {
    fn default() -> Self {
        Self::new()
    }
}

impl Broker {
    pub fn new() -> Broker {
        let (tx, rx) = channel();
        Broker {
            tx,
            rx,
            location_manager: None,
            pending_locations: Vec::new(),
            pending_geo: HashMap::new(),
            limits: limits::AbuseLimiter::default(),
            dialog_owner: HashMap::new(),
            notable: HashMap::new(),
            shown: HashMap::new(),
        }
    }

    /// Marks a request whose error answer the user must see.
    pub fn note(&mut self, reply: Reply, app_id: &str) {
        self.notable.insert((reply.heap_key, reply.req_id), app_id.to_string());
    }

    /// The refusals and failed actions answered since the last call, as
    /// exact isolate, for task status and host notifications. The same
    /// message from the same app gets a popup at most every 30 seconds.
    pub fn failures(&mut self) -> Vec<ServiceFailure> {
        let answers = ANSWERS.with(|a| std::mem::take(&mut *a.borrow_mut()));
        let now = std::time::Instant::now();
        let mut failed = Vec::new();
        for (reply, error) in answers {
            let app = self.notable.remove(&(reply.heap_key, reply.req_id));
            let (Some(app), Some(error)) = (app, error) else { continue };
            let key = (app, error);
            let fresh = self.shown.get(&key).is_none_or(|at| now.duration_since(*at).as_secs() >= 30);
            if fresh {
                self.shown.insert(key.clone(), now);
            }
            failed.push(ServiceFailure { app_id: key.0, heap_key: reply.heap_key, error: key.1, show_popup: fresh });
        }
        failed
    }

    /// Drop pending bookkeeping for a retired isolate, keeping answered errors
    /// until the host has attributed them to their completed background run.
    pub fn forget_instance(&mut self, heap_key: usize) {
        let answered = ANSWERS.with(|answers| answers.borrow().iter()
            .map(|(reply, _)| (reply.heap_key, reply.req_id)).collect::<HashSet<_>>());
        self.notable.retain(|key, _| key.0 != heap_key || answered.contains(key));
        self.pending_locations.retain(|reply| reply.heap_key != heap_key);
        self.pending_geo.retain(|_, reply| reply.heap_key != heap_key);
        // An OS dialog, if one exists, retains its guard until it actually
        // closes. Splash request IDs are unique across isolates; a late answer
        // cannot find a new worker's callback with a reused heap address.
    }

    /// Drops an app's rate-limit budget, strikes and dialog guard. Called when
    /// its isolates are torn down (force stop, uninstall, or the user allowing
    /// a restricted app to run again) — a fresh run starts with a clean sheet,
    /// while the persisted restriction is what remembers the abuse.
    pub fn forget_app(&mut self, app_id: &str) {
        self.limits.forget(app_id);
        self.dialog_owner.retain(|_, owner| owner != app_id);
        let answered = ANSWERS.with(|answers| answers.borrow().iter()
            .map(|(reply, _)| (reply.heap_key, reply.req_id)).collect::<HashSet<_>>());
        self.notable.retain(|key, owner| owner != app_id || answered.contains(key));
        self.shown.retain(|(owner, _), _| owner != app_id);
    }

    /// How many of this app's requests have been refused this run, for the
    /// app's info page.
    pub fn refusal_count(&self, app_id: &str) -> u64 {
        self.limits.refusals(app_id)
    }

    /// Marks an app's OS dialog as on screen, turning on the one-at-a-time
    /// guard — for hosts that raise a modal on an app's behalf.
    pub fn dialog_started(&mut self, app_id: &str) {
        self.limits.dialog_started(app_id);
    }

    /// Marks that dialog as finished (answered, cancelled or failed).
    pub fn dialog_finished(&mut self, app_id: &str) {
        self.limits.dialog_finished(app_id);
    }

    /// One full pass: everything isolates asked since the last event, plus
    /// every async completion that came home. Call once per `handle_event`.
    pub fn process(&mut self, cx: &mut Cx, ctx: BrokerCtx) -> Vec<BrokerAsk> {
        let mut asks = Vec::new();
        // Apps condemned during THIS drain. Once the host has decided to
        // stop an app, the rest of its batch is dropped rather than answered:
        // every answer is a synchronous re-entry into an isolate that is about
        // to be torn down in this same event pass, and a script that answers
        // by touching its UI leaves paused threads and queued widget calls
        // behind it. Dropping is the bridge's documented behaviour for an
        // undrained request — it simply never resolves — and the callbacks are
        // reaped with the isolate moments later.
        let mut condemned: Vec<MiniAppId> = Vec::new();
        for req in take_splash_host_requests() {
            let (app_part, _) = crate::manifest::split_instance_tag(&req.app_tag);
            if condemned.iter().any(|c| c == app_part) {
                continue;
            }
            let before = asks.len();
            self.dispatch(cx, ctx, req, &mut asks, Charge::Yes);
            for ask in &asks[before..] {
                if let BrokerAsk::Restrict { app_id, .. } = ask {
                    condemned.push(app_id.clone());
                }
            }
        }
        self.drain_completions(cx, ctx.check_response);
        asks
    }

    /// Re-runs a parked request after the user granted its permission. Not
    /// charged again: the app asked once, and the delay since was the user
    /// reading a prompt.
    pub fn dispatch_after_grant(&mut self, cx: &mut Cx, ctx: BrokerCtx, req: SplashHostRequest) -> Vec<BrokerAsk> {
        let mut asks = Vec::new();
        self.dispatch(cx, ctx, req, &mut asks, Charge::No);
        asks
    }

    /// The user just said no to this request's prompt, or dismissed it for
    /// the session: they know, so its refusal gets no popup.
    pub fn declined(&mut self, req: &SplashHostRequest) {
        self.notable.remove(&(req.heap_key, req.req_id));
    }

    /// Answers a parked request after the user denied its permission.
    /// `permissions.request` gets its documented `{granted: false}` shape (a
    /// denial IS its answer); everything else gets the error.
    pub fn respond_denied(cx: &mut Cx, req: &SplashHostRequest) {
        if req.service == "permissions.request" {
            return respond(cx, Reply::of(req), Ok("{\"granted\": false}"));
        }
        // Name the exact capability, so an author learns which single
        // ability is blocked rather than just its group.
        let msg = match crate::capabilities::for_service(&req.service) {
            Some(cap) => format!("The request to use \"{}\" was not approved.", cap.title),
            None => "permission denied".to_string(),
        };
        respond(cx, Reply::of(req), Err(&msg));
    }

    fn respond_policy_denied(cx: &mut Cx, req: &SplashHostRequest, store: &PermissionStore,
        manifest: &crate::manifest::MiniAppManifest, capability: &crate::capabilities::Capability,
        context: PermissionContext<'_>)
    {
        if req.service == "permissions.request" { return Self::respond_denied(cx, req); }
        let evaluation = store.capability_evaluation_for_in_context(&manifest.id,
            |permission| manifest.declares(permission), |cap| manifest.declares_capability(cap), capability, context);
        respond(cx, Reply::of(req), Err(&evaluation.public_message()));
    }

    fn dispatch(
        &mut self,
        cx: &mut Cx,
        ctx: BrokerCtx,
        req: SplashHostRequest,
        asks: &mut Vec<BrokerAsk>,
        charge: Charge,
    ) {
        if trace_on() {
            trace_line(&format!(
                "dispatch app={} svc={} heap={} req={} prompt={}\n",
                req.app_tag,
                req.service,
                req.heap_key,
                req.req_id,
                req.may_prompt,
            ));
        }
        let reply = Reply::of(&req);
        // Untagged isolates (previews, validation dry-runs) get a clean
        // refusal; the tag is host-assigned, so this cannot be spoofed away.
        if req.app_tag.is_empty() {
            return respond(cx, reply, Err("host services are not available here"));
        }
        // The tag names one INSTANCE: the app plus the room it is bound to.
        let (app_part, room_part) = crate::manifest::split_instance_tag(&req.app_tag);
        let instance_room = room_part.map(str::to_string);
        let Some(manifest) = ctx.registry.get(app_part).cloned() else {
            return respond(cx, reply, Err("unknown app"));
        };
        // A restricted app has already been stopped, so anything still queued
        // from it is a leftover from the isolate that was torn down. Drop it
        // without answering: there is nothing alive to answer, and re-entering
        // a dead isolate's heap is how you corrupt someone else's.
        if ctx.permissions.is_restricted(&manifest.id) {
            if trace_on() {
                trace_line(&format!(
                    "drop app={} svc={} reason=restricted\n",
                    manifest.id, req.service
                ));
            }
            return;
        }
        // Abuse control comes BEFORE the permission check, because refusing a
        // request is itself work and an app in a tight loop must not be able
        // to make the host do it forever.
        let on_screen = (ctx.pane_state)(req.heap_key).map_or_else(
            || ctx.foreground_app == Some(manifest.id.as_str()) || (ctx.is_docked)(&manifest.id),
            |pane| pane.foreground,
        );
        let may_prompt = on_screen && req.may_prompt;
        if charge == Charge::Yes {
            let foreground = on_screen;
            let verdict = self.limits.check(&manifest.id, &req.service, foreground);
            if trace_on() {
                if let limits::Verdict::Refuse(why) | limits::Verdict::Stop(why) = &verdict {
                    trace_line(&format!(
                        "limit app={} svc={} refused={:?} stop={} total_refusals={}\n",
                        manifest.id,
                        req.service,
                        why,
                        matches!(verdict, limits::Verdict::Stop(_)),
                        self.limits.refusals(&manifest.id),
                    ));
                }
            }
            match verdict {
                limits::Verdict::Allow => {}
                limits::Verdict::Refuse(why) => return respond(cx, reply, Err(why.message())),
                limits::Verdict::Stop(why) => {
                    // Answer the offending call, then hand the app to the host
                    // to be stopped: the broker cannot tear down widgets.
                    respond(cx, reply, Err(why.message()));
                    asks.push(BrokerAsk::Restrict {
                        app_id: manifest.id.clone(),
                        reason: "made far too many requests to the system".to_string(),
                    });
                    return;
                }
            }
        }
        let args: serde_json::Value =
            serde_json::from_str(&req.args_json).unwrap_or(serde_json::Value::Null);

        // Same-app IPC is inside one sandbox: no permission involved.
        let ipc_target = (matches!(req.service.as_str(), "ipc.send" | "ipc.post")).then(|| {
            let to = args["to"].as_str().unwrap_or_default().to_string();
            if to.is_empty() || to == "self" { manifest.id.clone() } else { to }
        });
        let Some(capability) = crate::capabilities::for_service(&req.service) else {
            self.note(reply, &manifest.id);
            return respond(cx, reply, Err(&format!("unknown service '{}'", req.service)));
        };
        // Any refusal from here on is the host's to show; an act (what a
        // button does) stays notable until its answer, so a failure shows too.
        self.note(reply, &manifest.id);
        let acts = matches!(capability.access, crate::capabilities::Access::Write | crate::capabilities::Access::ReadWrite | crate::capabilities::Access::Act)
            && !matches!(req.service.as_str(), "events.subscribe" | "events.unsubscribe" | "permissions.request" | "notify.clear" | "background.complete");
        if !capability.is_available() {
            return respond(cx, reply, Err(&format!("'{}' is not available in this Robrix", req.service)));
        }
        if capability.flow_contract().is_none() {
            return respond(cx, reply, Err("This service has no information-flow contract."));
        }
        let context = permission_context(&req.service, &args, instance_room.as_deref());
        let collection = is_room_collection(capability);
        let denied = if collection && req.service != "matrix.space_rooms" {
            ctx.permissions.global_policy(RoomAccess::Read) == PolicyDecision::Deny
        } else {
            ctx.permissions.capability_room_policy(capability, context) == PolicyDecision::Deny
        };
        if denied
        {
            return Self::respond_policy_denied(cx, &req, ctx.permissions, &manifest, capability, context);
        }
        // Same-app IPC is inside one sandbox: no permission involved.
        let self_ipc = matches!(req.service.as_str(), "ipc.send" | "ipc.post")
            && ipc_target.as_deref() == Some(manifest.id.as_str());
        // mcp.tools.* carry their own per-tool consent (the registration
        // prompt shows the app's exact description), so they bypass the
        // generic group gate and decide inside their match arms.
        let per_tool_service = req.service.starts_with("mcp.tools.");
        let needs = if self_ipc || per_tool_service { None } else { capability.group };
        if let Some(perm) = needs {
            // The group answers the prompt; the user can still block this
            // single capability underneath it.
            let effective = if perm == Permission::Network && req.service == "network.http" {
                if !manifest.declares(perm) { Effective::Undeclared }
                else if ctx.permissions.is_restricted(&manifest.id)
                    || ctx.permissions.state(&manifest.id, perm) == crate::permissions::GrantState::Denied
                    || ctx.permissions.capability_state(&manifest.id, capability.id) == crate::permissions::GrantState::Denied
                { Effective::Denied }
                else if ctx.permissions.is_url_allowed(&manifest.id, args["url"].as_str().unwrap_or(""), context) { Effective::Granted }
                else { Effective::NeedsPrompt }
            } else if collection {
                ctx.permissions.effective_collection_capability_in_context(&manifest, capability, context)
            } else {
                ctx.permissions.effective_capability_in_context(&manifest, capability, context)
            };
            match effective {
                // A capability actually being exercised — the only place a
                // "used" record can honestly come from.
                Effective::Granted => {
                    asks.push(BrokerAsk::Used { app_id: manifest.id.clone(), perm });
                }
                Effective::Denied => return Self::respond_policy_denied(cx, &req, ctx.permissions, &manifest, capability, context),
                Effective::Undeclared => {
                    return respond(
                        cx,
                        reply,
                        Err(&format!("permission not declared: {}", perm.as_str())),
                    );
                }
                Effective::NeedsPrompt => {
                    // Surfaces that may not prompt never pop consent dialogs:
                    // their Ask-state requests fail cleanly and the script
                    // falls back.
                    if !may_prompt {
                        return Self::respond_policy_denied(cx, &req, ctx.permissions, &manifest, capability, context);
                    }
                    asks.push(BrokerAsk::Prompt {
                        app_id: manifest.id.clone(),
                        perm,
                        request: Some(req),
                        tool: None,
                    });
                    return;
                }
            }
        }

        if !acts {
            self.notable.remove(&(reply.heap_key, reply.req_id));
        }

        if let Err(error) = (ctx.check_flow)(&req, capability, &args, ctx.registry) {
            return respond(cx, reply, Err(&error));
        }

        match req.service.as_str() {
            "background.complete" => {
                let Some(run_id) = args["run_id"].as_u64().filter(|id| *id > 0) else {
                    return respond(cx, reply, Err("background.complete needs a positive {run_id} from on_background"));
                };
                let Some(success) = args["success"].as_bool() else {
                    return respond(cx, reply, Err("background.complete needs {success: true} or {success: false}"));
                };
                asks.push(BrokerAsk::BackgroundComplete { reply, run_id, success });
            }
            "network.http" => asks.push(BrokerAsk::Network {
                reply, app_id: manifest.id.clone(), room: instance_room, args,
                consent: Box::new(ctx.permissions.clone()),
            }),
            "env" => {
                let pane = (ctx.pane_state)(req.heap_key);
                let visible_room = instance_room.as_deref().filter(|room| {
                    ctx.permissions.room_policy(Some(room), RoomAccess::Read) != PolicyDecision::Deny
                });
                let data = serde_json::json!({
                    "app_id": manifest.id,
                    "room_attached": instance_room.is_some(),
                    "room_id": visible_room,
                    "room_name": visible_room.and_then(|r| (ctx.room_name)(r)),
                    "instance_tag": if visible_room.is_some() { &req.app_tag } else { &manifest.id },
                    "surface": pane.as_ref().map_or("parked", |p| p.surface),
                    "platform": PLATFORM,
                    "view_mode": if ctx.desktop_view { "desktop" } else { "mobile" },
                });
                respond(cx, reply, Ok(&data.to_string()));
            }
            "ui.pane.read" => match (ctx.pane_state)(req.heap_key) {
                Some(pane) => respond(cx, reply, Ok(&pane.to_json().to_string())),
                None => respond(cx, reply, Err("this instance has no pane")),
            },
            "ui.pane.close" | "ui.pane.set_side" | "ui.pane.minimize" | "ui.pane.break_out" => {
                let action = match req.service.as_str() {
                    "ui.pane.close" => Ok(HostAction::ClosePane),
                    "ui.pane.minimize" => Ok(HostAction::Minimize),
                    "ui.pane.break_out" => Ok(HostAction::BreakOut),
                    _ => args["side"].as_str().and_then(PaneSide::from_str)
                        .map(|side| HostAction::SetSide { side })
                        .ok_or("ui.pane.set_side needs {side: top | bottom | left | right}"),
                };
                match action {
                    Ok(action) => asks.push(BrokerAsk::HostAction { reply, app_id: manifest.id.clone(), action }),
                    Err(e) => respond(cx, reply, Err(e)),
                }
            }
            "host.prefs" => asks.push(BrokerAsk::HostQuery { reply, query: HostQuery::Prefs }),
            "device.info" => asks.push(BrokerAsk::HostQuery { reply, query: HostQuery::DeviceInfo }),
            "ipc.apps_list" => {
                let apps: Vec<serde_json::Value> = ctx.registry.iter()
                    .filter(|m| m.declares(Permission::Ipc))
                    .map(|m| serde_json::json!({
                        "app_id": m.id,
                        "name": m.name,
                        "running": (ctx.is_running)(&m.id),
                    }))
                    .collect();
                respond(cx, reply, Ok(&serde_json::json!({ "apps": apps }).to_string()));
            }
            "storage.quota" => {
                // Walks the jail off-thread; there is no byte cap today.
                let dir = match (ctx.storage_path)(req.heap_key) {
                    Ok(dir) => dir,
                    Err(error) => return respond(cx, reply, Err(&error)),
                };
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let data = serde_json::json!({ "used": dir_bytes(&dir), "cap": serde_json::Value::Null });
                    tx.send(Completion::Respond(reply, Ok(data.to_string()))).ok();
                    SignalToUI::set_ui_signal();
                });
            }
            "permissions.query" => {
                let mut map = serde_json::Map::new();
                for (perm, _) in ctx.permissions.declared_states(&manifest) {
                    let s = match permission_request_status(ctx.permissions, &manifest, perm, context) {
                        Effective::Granted => "granted",
                        Effective::Denied => "denied",
                        _ => "ask",
                    };
                    map.insert(perm.as_str().to_string(), s.into());
                }
                respond(cx, reply, Ok(&serde_json::Value::Object(map).to_string()));
            }
            "permissions.request" => {
                let Some(perm) = args["perm"].as_str().and_then(Permission::from_str) else {
                    return respond(cx, reply, Err("unknown permission"));
                };
                match permission_request_status(ctx.permissions, &manifest, perm, context) {
                    Effective::Granted => respond(cx, reply, Ok("{\"granted\": true}")),
                    Effective::Denied => respond(cx, reply, Ok("{\"granted\": false}")),
                    Effective::Undeclared => {
                        respond(cx, reply, Err(&format!("permission not declared: {}", perm.as_str())));
                    }
                    Effective::NeedsPrompt if !may_prompt => {
                        respond(cx, reply, Ok("{\"granted\": false}"));
                    }
                    Effective::NeedsPrompt => {
                        asks.push(BrokerAsk::Prompt {
                            app_id: manifest.id.clone(),
                            perm,
                            request: Some(req),
                            tool: None,
                        });
                    }
                }
            }
            "location.get" => self.location_get(cx, reply),
            "clipboard.read" => {
                // Off-thread: pbpaste is usually instant but it IS a child
                // process, and a wedged pasteboard must not stall the UI.
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let out = read_clipboard()
                        .map(|text| serde_json::json!({ "text": text }).to_string());
                    tx.send(Completion::Respond(reply, out)).ok();
                    SignalToUI::set_ui_signal();
                });
            }
            "clipboard.write" => {
                let Some(text) = args["text"].as_str() else {
                    return respond(cx, reply, Err("clipboard.write needs {text}"));
                };
                cx.copy_to_clipboard(text);
                respond(cx, reply, Ok("{}"));
            }
            "url.open" => {
                let Some(url) = args["url"].as_str() else {
                    return respond(cx, reply, Err("url.open needs {url}"));
                };
                // Scheme allowlist: `file:` and friends reach places a
                // sandboxed app must not send the user.
                let ok_scheme = ["http://", "https://", "mailto:"]
                    .iter()
                    .any(|s| url.starts_with(s));
                if !ok_scheme || url.len() > 2048 {
                    return respond(cx, reply, Err("only http(s) and mailto links can be opened"));
                }
                match robius_open::Uri::new(url).open() {
                    Ok(()) => respond(cx, reply, Ok("{}")),
                    Err(e) => respond(cx, reply, Err(&format!("couldn't open link: {e:?}"))),
                }
            }
            "share" => {
                let Some(text) = args["text"].as_str() else {
                    return respond(cx, reply, Err("share needs {text}"));
                };
                match robius_share::ShareSheet::new().add_text(text).share() {
                    Ok(()) => respond(cx, reply, Ok("{}")),
                    Err(e) => respond(cx, reply, Err(&format!("couldn't share: {e:?}"))),
                }
            }
            "notify.post" => {
                // Popup text is the app's own words rendered in host chrome,
                // so it is flattened and clamped like a permission reason.
                let title = args["title"].as_str().unwrap_or("").trim();
                let body = args["body"].as_str().unwrap_or("").trim();
                let raw = match (title.is_empty(), body.is_empty()) {
                    (false, false) => format!("{title}: {body}"),
                    (false, true) => title.to_string(),
                    (true, false) => body.to_string(),
                    (true, true) => match args["count"].as_u64() {
                        Some(n) => format!("{} notifications", n.min(999)),
                        None => "New notification".to_string(),
                    },
                };
                let summary: String =
                    raw.replace(['\n', '\r'], " ").chars().take(200).collect();
                asks.push(BrokerAsk::Notify { app_id: manifest.id.clone(), summary });
                respond(cx, reply, Ok("{}"));
            }
            "notify.clear" => {
                // A shown popup can't be recalled; answered ok so scripts
                // written against the badge-count host keep working.
                respond(cx, reply, Ok("{}"));
            }
            "files.pick" => {
                let tx = self.tx.clone();
                let result = robius_file_picker::FileDialog::new().pick_file(move |res| {
                    tx.send(Completion::Respond(reply, picked_to_json(res))).ok();
                    SignalToUI::set_ui_signal();
                });
                match result {
                    Ok(()) => self.dialog_launched(&manifest.id, reply),
                    Err(e) => {
                        respond(cx, reply, Err(&format!("couldn't open the file picker: {e:?}")))
                    }
                }
            }
            "files.save" => {
                let Some(name) = args["name"].as_str().filter(|n| !n.is_empty()) else {
                    return respond(cx, reply, Err("files.save needs {name, data}"));
                };
                let Some(data) = args["data"].as_str() else {
                    return respond(cx, reply, Err("files.save needs {name, data}"));
                };
                let tx = self.tx.clone();
                let bytes = data.as_bytes().to_vec();
                let result = robius_file_picker::FileDialog::new()
                    .set_file_name(name)
                    .save_data(bytes, move |res| {
                        let out = match res {
                            Ok(Some(_)) => Ok("{\"saved\": true}".to_string()),
                            Ok(None) => Ok("{\"saved\": false}".to_string()),
                            Err(e) => Err(format!("save failed: {e:?}")),
                        };
                        tx.send(Completion::Respond(reply, out)).ok();
                        SignalToUI::set_ui_signal();
                    });
                match result {
                    Ok(()) => self.dialog_launched(&manifest.id, reply),
                    Err(e) => {
                        respond(cx, reply, Err(&format!("couldn't open the save dialog: {e:?}")))
                    }
                }
            }
            "auth.check" => {
                let reason = args["reason"].as_str().unwrap_or("Confirm it's you").to_string();
                if self.auth_check(cx, reply, &manifest.name, &reason) {
                    self.dialog_launched(&manifest.id, reply);
                }
            }
            "ipc.send" | "ipc.post" => {
                if args.get("to").is_some_and(|value| !value.is_string()) {
                    return respond(cx, reply, Err("IPC needs a string destination."));
                }
                let to = ipc_target.unwrap_or_default();
                let data_json = args["data"].to_string();
                let receipt = req.service == "ipc.send";
                // Post acknowledges syntax/consent only. Neither absent targets
                // nor denied/closed receivers may become a public data source.
                if !receipt { respond(cx, reply, Ok("{\"accepted\":true}")); }
                if to != manifest.id {
                    let Some(target) = ctx.registry.get(&to) else {
                        if receipt { respond(cx, reply, Err("no such app")); }
                        return;
                    };
                    let blocked = !target.declares(Permission::Ipc)
                        || ctx.permissions.is_restricted(&to)
                        || ctx.permissions.state(&to, Permission::Ipc)
                            == crate::permissions::GrantState::Denied;
                    if blocked {
                        if receipt { respond(cx, reply, Err("that app doesn't accept messages")); }
                        return;
                    }
                }
                asks.push(BrokerAsk::IpcDeliver {
                    reply,
                    from: manifest.id.clone(),
                    from_heap: req.heap_key,
                    to,
                    data_json,
                    receipt,
                });
            }
            "mcp.tools.register" => match parse_app_tool_request(&manifest, &req, &args) {
                Err(e) => respond(cx, reply, Err(&e)),
                Ok(tool) => {
                    // The group is a kill switch (a durable Deny in App Info
                    // blocks every tool this app might offer), and its
                    // per-capability row can block registration underneath
                    // it: the same gate every other service gets, minus the
                    // prompt, which is per tool below.
                    match ctx.permissions.effective_capability_in_context(&manifest, capability, context) {
                        Effective::Undeclared => {
                            return respond(cx, reply, Err(&format!(
                                "permission not declared: {}", Permission::McpTools.as_str()
                            )));
                        }
                        Effective::Denied => return Self::respond_policy_denied(cx, &req, ctx.permissions, &manifest, capability, context),
                        Effective::Granted | Effective::NeedsPrompt => {}
                    }
                    let tool_effective = ctx.permissions.tool_effective(
                        &manifest.id,
                        &tool.full_name,
                        Some(&tool.content_hash),
                    );
                    let tool_effective = if tool_effective == Effective::NeedsPrompt
                        && ctx.permissions.has_scoped_tool_grant(&manifest.id, &tool.full_name, &tool.content_hash, context)
                    { Effective::Granted } else { tool_effective };
                    match tool_effective {
                        Effective::Granted => {
                            asks.push(BrokerAsk::Used {
                                app_id: manifest.id.clone(),
                                perm: Permission::McpTools,
                            });
                            asks.push(BrokerAsk::McpRegisterTool { reply, request: tool });
                        }
                        Effective::NeedsPrompt if may_prompt => {
                            asks.push(BrokerAsk::Prompt {
                                app_id: manifest.id.clone(),
                                perm: Permission::McpTools,
                                request: Some(req),
                                tool: Some(tool),
                            });
                        }
                        // Denied, or a prompt this surface may not show.
                        _ => respond(cx, reply, Err(&format!(
                            "the tool \"{}\" is not allowed for this app",
                            tool.name
                        ))),
                    }
                }
            },
            "mcp.tools.unregister" => {
                if !manifest.declares(Permission::McpTools) {
                    return respond(cx, reply, Err(&format!(
                        "permission not declared: {}", Permission::McpTools.as_str()
                    )));
                }
                let Some(name) = args["name"].as_str().map(str::trim).filter(|n| !n.is_empty())
                else {
                    return respond(cx, reply, Err("mcp.tools.unregister needs a {name}"));
                };
                let full_name = app_tool_full_name(&manifest.id, name);
                asks.push(BrokerAsk::McpUnregisterTool {
                    reply,
                    app_id: manifest.id.clone(),
                    heap_key: req.heap_key,
                    full_name,
                });
            }
            "mcp.tools.result" => {
                let Some(call_id) = args["call_id"].as_u64() else {
                    return respond(cx, reply, Err("mcp.tools.result needs a numeric {call_id}"));
                };
                let ok = args["ok"].as_bool().unwrap_or(false);
                let text = app_tool_result_text(&args["result"]);
                asks.push(BrokerAsk::McpToolResult {
                    reply,
                    app_id: manifest.id.clone(),
                    call_id,
                    ok,
                    text,
                });
            }
            service if matrix::is_service(service) => {
                match matrix::parse(service, &args, instance_room.is_some()) {
                    Ok(call) => asks.push(BrokerAsk::Matrix {
                        reply,
                        app_id: manifest.id.clone(),
                        room: instance_room.clone(),
                        capability: capability.id,
                        consent: Box::new(ctx.permissions.clone()),
                        call,
                    }),
                    Err(e) => respond(cx, reply, Err(&e)),
                }
            }
            "events.subscribe" => {
                let Some(name) = args["event"].as_str() else {
                    return respond(cx, reply, Err("events.subscribe needs {event}"));
                };
                let Some(hook) = crate::capabilities::for_hook(name).filter(|c| c.is_available() && c.flow_contract().is_some()) else {
                    return respond(cx, reply, Err(&format!("unknown event '{name}'")));
                };
                let room = instance_room.clone();
                if room.is_none() && hook.scope == crate::capabilities::Scope::Room {
                    return respond(cx, reply, Err("this mini-app is not attached to a room"));
                }
                // The hook's own group answers, exactly like an outgoing call.
                let effective = if is_room_collection(hook) {
                    ctx.permissions.effective_collection_capability_in_context(&manifest, hook, context)
                } else {
                    ctx.permissions.effective_capability_in_context(&manifest, hook, context)
                };
                match effective {
                    Effective::Granted => {
                        if let Some(perm) = hook.group {
                            asks.push(BrokerAsk::Used { app_id: manifest.id.clone(), perm });
                        }
                        asks.push(BrokerAsk::Subscribe {
                            reply,
                            app_id: manifest.id.clone(),
                            heap_key: req.heap_key,
                            room,
                            hook: hook.wire[0],
                        });
                    }
                    Effective::Denied => respond(cx, reply, Err(&format!("\"{}\" is denied for this app. Allow it in App Info", hook.title))),
                    Effective::Undeclared => {
                        let group = hook.group.map_or("", |g| g.as_str());
                        respond(cx, reply, Err(&format!("permission not declared: {group}")));
                    }
                    Effective::NeedsPrompt if !may_prompt => {
                        respond(cx, reply, Err(&format!("\"{}\" is denied for this app. Allow it in App Info", hook.title)));
                    }
                    Effective::NeedsPrompt => {
                        let Some(perm) = hook.group else { return };
                        asks.push(BrokerAsk::Prompt {
                            app_id: manifest.id.clone(),
                            perm,
                            request: Some(req),
                            tool: None,
                        });
                    }
                }
            }
            "events.unsubscribe" => {
                let name = args["event"].as_str().unwrap_or("*");
                let hook = if name == "*" {
                    None
                } else {
                    let Some(hook) = crate::capabilities::for_hook(name) else {
                        return respond(cx, reply, Err(&format!("unknown event '{name}'")));
                    };
                    Some(hook.wire[0])
                };
                asks.push(BrokerAsk::Unsubscribe { reply, heap_key: req.heap_key, hook });
            }
            "nav.room" | "nav.event" | "nav.thread" | "nav.user" | "nav.space"
            | "nav.screen" | "nav.link" | "nav.app" | "composer.insert"
            | "composer.reply_to" => {
                let arg = |key: &str| args[key].as_str().map(str::trim).filter(|v| !v.is_empty());
                let room = arg("room_id").map(str::to_string).or_else(|| instance_room.clone());
                let need = |key: &str| arg(key).map(str::to_string).ok_or(format!("{} needs {{{key}}}", req.service));
                let action = match req.service.as_str() {
                    "nav.room" => need("room_id").map(|room| HostAction::OpenRoom { room }),
                    "nav.event" => need("event_id").map(|event_id| HostAction::JumpToEvent { room, event_id }),
                    "nav.thread" => need("event_id").map(|event_id| HostAction::OpenThread { room, event_id }),
                    "nav.user" => need("user_id").map(|user_id| HostAction::ShowUser { room, user_id }),
                    "nav.space" => need("space_id").map(|space| HostAction::OpenSpace { space }),
                    "nav.screen" => need("screen").map(|screen| HostAction::OpenScreen { screen }),
                    "nav.link" => need("url").map(|url| HostAction::OpenLink { room, url }),
                    "nav.app" => need("app_id").map(|app_id| HostAction::OpenApp { room, app_id }),
                    "composer.reply_to" => need("event_id").map(|event_id| HostAction::ComposerReplyTo { room, event_id }),
                    "composer.insert" => need("text").and_then(|text| {
                        // A draft is the app's words in the user's composer, so it
                        // gets the same ceiling a sent message does.
                        (text.chars().count() <= 4096)
                            .then_some(HostAction::ComposerInsert { room, text })
                            .ok_or_else(|| String::from("text is too long (4096 characters max)"))
                    }),
                    _ => unreachable!("service list checked above"),
                };
                match action {
                    Ok(action) => asks.push(BrokerAsk::HostAction {
                        reply,
                        app_id: manifest.id.clone(),
                        action,
                    }),
                    Err(e) => respond(cx, reply, Err(&e)),
                }
            }
            _ => respond(cx, reply, Err("This service has no broker implementation.")),
        }
    }

    /// CoreLocation first; on any failure every waiting request falls back to
    /// IP geolocation (city-level) via the host's own HTTP stack.
    fn location_get(&mut self, cx: &mut Cx, reply: Reply) {
        let already_waiting = !self.pending_locations.is_empty();
        self.pending_locations.push(reply);
        if already_waiting {
            return;
        }
        if self.location_manager.is_none() {
            self.location_manager = robius_location::Manager::new(LocationHandler {
                tx: Mutex::new(self.tx.clone()),
            })
            .ok();
        }
        let ok = self
            .location_manager
            .as_ref()
            .is_some_and(|m| m.update_once().is_ok());
        if !ok {
            // No manager (or a sync failure): go straight to the fallback.
            self.start_ip_geolocation(cx);
        }
    }

    fn start_ip_geolocation(&mut self, cx: &mut Cx) {
        for reply in std::mem::take(&mut self.pending_locations) {
            let id = LiveId::unique();
            self.pending_geo.insert(id, reply);
            cx.http_request(id, HttpRequest::new(GEO_URL.to_string(), HttpMethod::GET));
        }
    }

    /// Records that an OS dialog is now on screen for `app_id`, so the app
    /// cannot stack a second one behind it. Released in `drain_completions`
    /// when the user answers, cancels, or the dialog fails.
    fn dialog_launched(&mut self, app_id: &MiniAppId, reply: Reply) {
        self.limits.dialog_started(app_id);
        self.dialog_owner.insert((reply.heap_key, reply.req_id), app_id.clone());
    }

    /// Returns whether a prompt actually went up (so the caller knows whether
    /// to start the one-at-a-time guard); errors are answered here.
    fn auth_check(&mut self, cx: &mut Cx, reply: Reply, app_name: &str, reason: &str) -> bool {
        let Some(policy) = robius_authentication::PolicyBuilder::new().build() else {
            respond(cx, reply, Err("authentication is not available here"));
            return false;
        };
        let text = robius_authentication::Text {
            android: robius_authentication::AndroidText {
                title: app_name,
                subtitle: None,
                description: Some(reason),
            },
            apple: reason,
            // Truncating constructor: the reason is script-supplied, and the
            // plain new() panics past Windows' length caps.
            windows: robius_authentication::WindowsText::new_truncated(app_name, reason),
        };
        let tx = self.tx.clone();
        let context = robius_authentication::Context::new(());
        let result = context.authenticate(text, &policy, move |res| {
            let out = match res {
                Ok(()) => Ok("{}".to_string()),
                Err(e) => Err(format!("not authenticated: {e:?}")),
            };
            tx.send(Completion::Respond(reply, out)).ok();
            SignalToUI::set_ui_signal();
        });
        if let Err(e) = result {
            respond(cx, reply, Err(&format!("authentication unavailable: {e:?}")));
            return false;
        }
        true
    }

    fn drain_completions(&mut self, cx: &mut Cx, check_response: &dyn Fn(Reply, &str) -> Result<(), String>) {
        while let Ok(done) = self.rx.try_recv() {
            // Any answer to a modal service means its dialog is off screen,
            // whether the user accepted, cancelled, or it failed outright.
            if let Completion::Respond(reply, _) = &done {
                if let Some(owner) = self.dialog_owner.remove(&(reply.heap_key, reply.req_id)) {
                    self.limits.dialog_finished(&owner);
                }
            }
            match done {
                Completion::Respond(reply, Ok(json)) => respond_checked(cx, reply, &json, check_response),
                Completion::Respond(reply, Err(e)) => respond(cx, reply, Err(&e)),
                Completion::LocationFix { lat, lon } => {
                    let data = serde_json::json!({
                        "lat": lat, "lon": lon, "city": "", "source": "gps",
                    })
                    .to_string();
                    for reply in std::mem::take(&mut self.pending_locations) {
                        respond_checked(cx, reply, &data, check_response);
                    }
                }
                Completion::LocationFailed => self.start_ip_geolocation(cx),
            }
        }
    }

    /// Host-side network completions (the IP-geolocation fallback). Call from
    /// `Event::NetworkResponses`; unrelated request ids are left alone.
    pub fn handle_network(&mut self, cx: &mut Cx, responses: &NetworkResponsesEvent, check_response: &dyn Fn(Reply, &str) -> Result<(), String>) {
        for response in responses {
            let request_id = match response {
                NetworkResponse::HttpResponse { request_id, .. }
                | NetworkResponse::HttpError { request_id, .. } => *request_id,
                _ => continue,
            };
            let Some(reply) = self.pending_geo.remove(&request_id) else {
                continue;
            };
            match response {
                NetworkResponse::HttpResponse { response, .. } => {
                    let parsed = response
                        .get_string_body()
                        .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
                        .and_then(|v| geo_json_to_location(&v));
                    match parsed {
                        Some(data) => respond_checked(cx, reply, &data, check_response),
                        None => respond(cx, reply, Err("couldn't determine your location")),
                    }
                }
                _ => respond(cx, reply, Err("couldn't determine your location")),
            }
        }
    }
}

fn respond_checked(cx: &mut Cx, reply: Reply, data: &str, check_response: &dyn Fn(Reply, &str) -> Result<(), String>) {
    match check_response(reply, data) {
        Ok(()) => respond(cx, reply, Ok(data)),
        Err(error) => respond(cx, reply, Err(&error)),
    }
}


struct LocationHandler {
    tx: Mutex<Sender<Completion>>,
}

impl robius_location::Handler for LocationHandler {
    fn handle(&self, location: robius_location::Location<'_>) {
        let tx = self.tx.lock().unwrap();
        match location.coordinates() {
            Ok(coords) => {
                tx.send(Completion::LocationFix { lat: coords.latitude, lon: coords.longitude })
                    .ok();
            }
            // A fix without coordinates still has to answer the waiters, or
            // every later location.get piles behind them forever.
            Err(_) => {
                tx.send(Completion::LocationFailed).ok();
            }
        }
        SignalToUI::set_ui_signal();
    }

    fn error(&self, _error: robius_location::Error) {
        let tx = self.tx.lock().unwrap();
        tx.send(Completion::LocationFailed).ok();
        SignalToUI::set_ui_signal();
    }
}

fn dir_bytes(dir: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries.flatten().map(|entry| {
        match entry.metadata() {
            Ok(meta) if meta.is_dir() => dir_bytes(&entry.path()),
            Ok(meta) => meta.len(),
            Err(_) => 0,
        }
    }).sum()
}

/// Accepts every common IP-geolocation response shape (ipapi.co and
/// ip-api.com) and normalizes to the service's `{lat, lon, city}`.
fn geo_json_to_location(v: &serde_json::Value) -> Option<String> {
    let lat = v["lat"].as_f64().or_else(|| v["latitude"].as_f64())?;
    let lon = v["lon"].as_f64().or_else(|| v["longitude"].as_f64())?;
    let city = v["city"].as_str().unwrap_or("");
    Some(serde_json::json!({ "lat": lat, "lon": lon, "city": city, "source": "ip" }).to_string())
}

fn picked_to_json(
    res: Result<Option<robius_file_picker::PickedFile>, robius_file_picker::Error>,
) -> Result<String, String> {
    /// Read cap: results ride through a script heap as one string.
    const MAX_PICK_BYTES: u64 = 1024 * 1024;
    match res {
        Ok(None) => Ok("{\"cancelled\": true}".to_string()),
        Ok(Some(file)) => {
            // size() can be None on desktop; fall back to fs metadata so a
            // huge file is rejected BEFORE read_bytes buffers all of it.
            let size = file.size().or_else(|| {
                file.path()
                    .and_then(|p| std::fs::metadata(p).ok())
                    .map(|m| m.len())
            });
            if size.is_some_and(|n| n > MAX_PICK_BYTES) {
                return Err("file is too large (1MB max)".to_string());
            }
            let bytes = file.read_bytes().map_err(|e| format!("couldn't read the file: {e:?}"))?;
            if bytes.len() as u64 > MAX_PICK_BYTES {
                return Err("file is too large (1MB max)".to_string());
            }
            let text = String::from_utf8(bytes)
                .map_err(|_| "only text files are supported for now".to_string())?;
            let name = file.display_name().unwrap_or("file").to_string();
            Ok(serde_json::json!({
                "name": name,
                "size": text.len(),
                "text": text,
            })
            .to_string())
        }
        Err(e) => Err(format!("file picker failed: {e:?}")),
    }
}

/// Clipboard read has no makepad API; on macOS the system `pbpaste` is the
/// host-side (never script-side) door. 64KB cap keeps a giant clipboard from
/// flooding a script heap; bytes are capped BEFORE the lossy conversion so a
/// multibyte boundary can never panic a String::truncate.
fn read_clipboard() -> Result<String, String> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("/usr/bin/pbpaste")
            .output()
            .map_err(|e| format!("clipboard unavailable: {e}"))?;
        let mut bytes = out.stdout;
        bytes.truncate(64 * 1024);
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("clipboard read is not available on this platform".to_string())
    }
}

#[cfg(test)]
mod room_context_tests {
    use super::*;

    fn permission_manifest(permissions: &[Permission], capabilities: &[&str]) -> crate::manifest::MiniAppManifest {
        serde_json::from_value(serde_json::json!({
            "id": "test-app", "name": "Test", "icon": "t", "tint": 0,
            "source": "", "allow_net": false, "builtin": false,
            "permissions": permissions.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
            "capabilities": capabilities,
        })).unwrap()
    }

    #[test]
    fn attached_services_cannot_spoof_policy_target_with_ignored_room_argument() {
        let args = serde_json::json!({ "room_id": "!allowed:s" });
        let context = permission_context("matrix.read_messages", &args, Some("!protected:s"));
        assert_eq!(context.target_room, Some("!protected:s"));
        assert_eq!(context.origin_room, Some("!protected:s"));
        let context = permission_context("matrix.rooms_messages", &args, Some("!origin:s"));
        assert_eq!(context.target_room, Some("!allowed:s"));
        assert_eq!(context.origin_room, Some("!origin:s"));
    }

    #[test]
    fn space_and_alias_services_use_the_target_the_worker_will_resolve() {
        let args = serde_json::json!({ "space_id": " !space:s ", "room": "#alias:s" });
        assert_eq!(permission_context("matrix.space_rooms", &args, Some("!origin:s")).target_room, Some("!space:s"));
        assert_eq!(permission_context("matrix.room_preview", &args, Some("!origin:s")).target_room, Some("#alias:s"));
    }

    #[test]
    fn permission_requests_in_a_whitelisted_room_do_not_prompt_or_grant_other_rooms() {
        let manifest = permission_manifest(&[Permission::MatrixRoomRead, Permission::MatrixRoomSend], &[]);
        let mut store = PermissionStore::default();
        store.set_global_policy(RoomAccess::Write, PolicyDecision::Ask);
        store.set_room_policy("!testing:s", RoomAccess::Read, PolicyDecision::Allow);
        store.set_room_policy("!testing:s", RoomAccess::Write, PolicyDecision::Allow);
        let testing = PermissionContext { origin_room: Some("!testing:s"), target_room: Some("!testing:s") };
        let elsewhere = PermissionContext { origin_room: Some("!testing:s"), target_room: Some("!elsewhere:s") };
        for (permission, capability) in [
            (Permission::MatrixRoomRead, "matrix.room.messages.read"),
            (Permission::MatrixRoomSend, "matrix.room.message.send"),
        ] {
            assert_eq!(permission_request_status(&store, &manifest, permission, testing), Effective::Granted);
            assert_eq!(store.state(&manifest.id, permission), crate::permissions::GrantState::Ask);
            let capability = crate::capabilities::by_id(capability).unwrap();
            assert_eq!(store.effective_capability_in_context(&manifest, capability, elsewhere), Effective::NeedsPrompt);
        }
        store.set(&manifest.id, Permission::MatrixRoomRead, crate::permissions::GrantState::Denied);
        assert_eq!(permission_request_status(&store, &manifest, Permission::MatrixRoomRead, testing), Effective::Denied);
        assert_eq!(permission_request_status(&store, &manifest, Permission::Location, testing), Effective::Undeclared);
    }

    #[test]
    fn request_reports_only_declared_capabilities_and_preserves_individual_denials() {
        let manifest = permission_manifest(&[Permission::MatrixRoomRead], &["matrix.room.messages.read"]);
        let mut store = PermissionStore::default();
        let context = PermissionContext { origin_room: Some("!testing:s"), target_room: Some("!testing:s") };
        store.set_room_policy("!testing:s", RoomAccess::Read, PolicyDecision::Allow);
        store.set_capability(&manifest.id, "matrix.room.messages.read", crate::permissions::GrantState::Denied);
        assert_eq!(permission_request_status(&store, &manifest, Permission::MatrixRoomRead, context), Effective::Denied);
        let manifest = permission_manifest(&[Permission::MatrixRoomRead], &[]);
        assert_eq!(permission_request_status(&store, &manifest, Permission::MatrixRoomRead, context), Effective::Granted);
        let denied = crate::capabilities::by_id("matrix.room.messages.read").unwrap();
        assert_eq!(store.effective_capability_in_context(&manifest, denied, context), Effective::Denied);
        store.set_global_policy(RoomAccess::Read, PolicyDecision::Deny);
        assert_eq!(permission_request_status(&store, &manifest, Permission::MatrixRoomRead, context), Effective::Denied);
    }
}

#[cfg(test)]
mod app_tool_tests {
    use super::*;

    fn manifest(id: &str) -> crate::manifest::MiniAppManifest {
        crate::manifest::MiniAppManifest {
            id: id.into(),
            name: "Board".into(),
            icon: "t".into(),
            tint: 0,
            description: String::new(),
            source: String::new(),
            allow_net: false,
            permissions: vec!["mcp-tools".into()],
            permission_reasons: Default::default(),
            capabilities: Vec::new(),
            builtin: false,
            widget: None,
            shortcuts: vec![],
            scope: Default::default(),
            current_version: None,
        }
    }

    fn request(app_tag: &str, heap_key: usize) -> SplashHostRequest {
        SplashHostRequest {
            app_tag: app_tag.to_string(),
            heap_key,
            req_id: 1,
            service: "mcp.tools.register".to_string(),
            args_json: String::from("null"),
            may_prompt: true,
        }
    }

    #[test]
    fn tool_names_are_namespaced_and_sanitized() {
        assert_eq!(app_tool_full_name("my-app", "TTT_Play"), "app_my_app_ttt_play");
        assert_eq!(app_tool_full_name("board", "play"), "app_board_play");
    }

    #[test]
    fn a_valid_registration_parses_into_a_schema_and_hash() {
        let m = manifest("board");
        let args = serde_json::json!({
            "name": "play",
            "description": "Place a mark on the board.",
            "args": [{"name": "cell", "type": "integer", "description": "0-8"}],
        });
        let req = request("board@!room:server", 7);
        let parsed = parse_app_tool_request(&m, &req, &args).unwrap();
        assert_eq!(parsed.full_name, "app_board_play");
        assert_eq!(parsed.name, "play");
        assert_eq!(parsed.room.as_deref(), Some("!room:server"));
        assert_eq!(parsed.heap_key, 7);
        assert_eq!(parsed.schema["properties"]["cell"]["type"], "integer");
        assert_eq!(parsed.schema["required"], serde_json::json!(["cell"]));

        // A changed description produces a different hash, so the stored grant
        // no longer matches and the user is asked to review it again.
        let mut changed = args.clone();
        changed["description"] = serde_json::json!("Place a mark, but differently.");
        let parsed2 = parse_app_tool_request(&m, &req, &changed).unwrap();
        assert_ne!(parsed.content_hash, parsed2.content_hash);
    }

    /// `mcp.tools.*` skip the generic group gate, so the manifest declaration
    /// is checked here: an app that never asked for `mcp-tools` gets the same
    /// refusal the gate would give, before any prompt.
    #[test]
    fn undeclared_mcp_tools_is_refused_before_any_prompt() {
        let mut m = manifest("board");
        m.permissions.clear();
        let args = serde_json::json!({ "name": "play", "description": "Place a mark." });
        let err = parse_app_tool_request(&m, &request("board@!room:server", 1), &args).unwrap_err();
        assert_eq!(err, "permission not declared: mcp-tools");
    }

    /// A roomless instance has no AI session to land a tool on, so it is
    /// told so instead of being prompted for a grant that can never work.
    #[test]
    fn roomless_registrations_are_refused() {
        let m = manifest("board");
        let args = serde_json::json!({ "name": "play", "description": "Place a mark." });
        let err = parse_app_tool_request(&m, &request("board", 1), &args).unwrap_err();
        assert_eq!(err, "this mini-app is not attached to a room");
        assert!(parse_app_tool_request(&m, &request("board@!room:server", 1), &args).is_ok());
    }

    #[test]
    fn duplicate_argument_names_are_refused() {
        let m = manifest("board");
        let args = serde_json::json!({
            "name": "play",
            "description": "Place a mark.",
            "args": [{"name": "cell", "type": "integer"}, {"name": "cell", "type": "string"}],
        });
        let err = parse_app_tool_request(&m, &request("board@!room:server", 1), &args).unwrap_err();
        assert_eq!(err, "argument `cell` is declared twice");
    }

    #[test]
    fn tool_results_are_clamped_before_reaching_the_model() {
        let short = serde_json::json!("fine");
        assert_eq!(app_tool_result_text(&short), "fine");
        assert_eq!(app_tool_result_text(&serde_json::Value::Null), "");
        assert_eq!(app_tool_result_text(&serde_json::json!({"a": 1})), "{\"a\":1}");
        let long = "é".repeat(MAX_APP_TOOL_RESULT_CHARS + 5);
        let out = app_tool_result_text(&serde_json::json!(long));
        assert!(out.ends_with("… [truncated]"));
        assert_eq!(out.chars().count(), MAX_APP_TOOL_RESULT_CHARS + "… [truncated]".chars().count());
    }

    #[test]
    fn bad_registrations_are_refused() {
        let m = manifest("board");
        let req = request("board@!room:server", 1);
        assert!(parse_app_tool_request(&m, &req, &serde_json::json!({"description": "x"})).is_err());
        assert!(parse_app_tool_request(&m, &req, &serde_json::json!({"name": "x"})).is_err());
        assert!(
            parse_app_tool_request(&m, &req, &serde_json::json!({"name": "a b", "description": "x"}))
                .is_err()
        );
        let bad = serde_json::json!({
            "name": "x",
            "description": "y",
            "args": [{"name": "n", "type": "wat"}],
        });
        assert!(parse_app_tool_request(&m, &req, &bad).is_err());
    }
}

#[cfg(test)]
mod failure_context_tests {
    use super::*;

    #[test]
    fn repeated_errors_retain_each_context_while_popups_are_coalesced() {
        let mut broker = Broker::new();
        let one = Reply { heap_key: 1, req_id: 1 };
        let two = Reply { heap_key: 2, req_id: 1 };
        broker.note(one, "watcher");
        broker.note(two, "watcher");
        ANSWERS.with(|answers| *answers.borrow_mut() = vec![
            (one, Some("Room writes are disabled.".into())),
            (two, Some("Room writes are disabled.".into())),
        ]);
        broker.note(Reply { heap_key: 1, req_id: 2 }, "watcher");
        broker.forget_instance(1);
        assert!(!broker.notable.contains_key(&(1, 2)), "abandoned requests must not accumulate");
        broker.forget_app("watcher");
        let failures = broker.failures();
        assert_eq!(failures.len(), 2);
        assert_eq!((failures[0].heap_key, failures[1].heap_key), (1, 2));
        assert!(failures[0].show_popup);
        assert!(!failures[1].show_popup);
        assert_eq!(failures[0].error, failures[1].error);
        assert!(broker.failures().is_empty());
    }
}
