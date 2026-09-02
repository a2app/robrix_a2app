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
use a2app_core::permissions::{Effective, GrantState, Permission, PermissionStore};
use a2app_core::persistence::{self, A2AppPersistedState};
use a2app_core::services::{self, Broker, BrokerAsk, BrokerCtx, HostAction, MatrixServiceCall, Reply, MATRIX_WRITE_OFF_MSG};
use a2app_agent::pipeline::{GenOutcome, Generation};
use a2app_agent::prefs::AgentPrefs;

use crate::a2app::host_pane::{MiniAppHostPaneAction, MiniAppHostPaneWidgetRefExt};
use crate::a2app::permission_prompt::{
    MiniAppPermissionPromptWidgetRefExt, PermissionPromptAction, PromptInfo,
};
use crate::a2app::dock::DockCmd;
use crate::a2app::instances::{self, InstanceKey, MiniAppInstanceAction};
use crate::a2app::room_watch::{self, A2AppRoomWatchEvent, RoomWatchKind};
use a2app_core::layout::PaneLayout;
use crate::app::{AppStateAction, SelectedRoom};
use crate::home::navigation_tab_bar::NavigationBarAction;
use crate::home::rooms_list::{RoomsListAction, RoomsListRef};
use crate::room::BasicRoomDetails;
use crate::shared::popup_list::{enqueue_popup_notification, PopupKind};
use crate::sliding_sync::{submit_async_request, MatrixRequest};
use crate::utils::RoomNameId;

/// How long between saves of dirty permission/registry state.
const PERSIST_THROTTLE: Duration = Duration::from_secs(2);
/// How often expired timed grants are checked for.
const TIMED_GRANT_CHECK: Duration = Duration::from_secs(5);
/// How often the generation console re-renders while output streams in.
const CONSOLE_REPAINT: Duration = Duration::from_millis(120);
/// How long a room-scoped host action waits for its RoomScreen to appear.
const ROOM_ACTION_TTL: Duration = Duration::from_secs(10);

thread_local! {
    static A2APP: RefCell<Option<A2AppState>> = const { RefCell::new(None) };
}

/// Runs `f` against the global a2app state, if it has been initialized.
pub fn with_a2app<R>(f: impl FnOnce(&mut A2AppState) -> R) -> Option<R> {
    A2APP.with(|state| state.borrow_mut().as_mut().map(f))
}

/// A runtime permission prompt waiting for (or showing to) the user.
pub struct PermissionPrompt {
    pub app_id: MiniAppId,
    pub perm: Permission,
    /// Bridge requests parked behind this prompt; replayed or refused
    /// once the user answers.
    pub parked: Vec<SplashHostRequest>,
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

/// All a2app state, owned by the UI thread.
pub struct A2AppState {
    pub registry: AppRegistry,
    pub permissions: PermissionStore,
    pub persisted: A2AppPersistedState,
    pub broker: Broker,
    pub prompts: VecDeque<PermissionPrompt>,
    pub active_prompt: Option<PermissionPrompt>,
    /// (app, permission) pairs the user said "Not Now" to this session.
    pub dismissed_prompts: HashSet<(MiniAppId, Permission)>,
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
    perms_dirty: bool,
    registry_dirty: bool,
    last_persist: Instant,
    last_timed_check: Instant,
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
    // A user's saved copy of an app (including a modified builtin) shadows
    // the stock one.
    for app in persistence::load_user_apps() {
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
            generation: None,
            console: GenConsole::default(),
            failed_request: None,
            agent_prefs: a2app_agent::prefs::load_agent_prefs(),
            create_room: None,
            foreground_app: None,
            room_action: None,
            hook_subs: HashMap::new(),
            watched_rooms: HashSet::new(),
            perms_dirty: false,
            registry_dirty: false,
            last_persist: Instant::now(),
            last_timed_check: Instant::now(),
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
    RestoreVersion { app_id: MiniAppId, stamp: String },
    SetPermission { app_id: MiniAppId, perm: Permission, state: GrantState },
    /// A single ability's own answer under its group (`Ask` = follow group).
    SetCapability { app_id: MiniAppId, cap_id: String, state: GrantState },
    Unrestrict(MiniAppId),
    /// Starts a generation; `Modify` intent is classified from the text.
    StartGeneration { request: String, room_id: Option<OwnedRoomId> },
    StartModify { app_id: MiniAppId, request: String },
    CancelGeneration,
    RetryGeneration,
    /// Clears the finished console back to the composer.
    NewPrompt,
    /// Posts an app's bundle into a room as a custom event.
    ShareToRoom { app_id: MiniAppId, room_id: OwnedRoomId },
    /// The user's "Mini-apps may write to rooms" switch.
    SetMatrixWrite(bool),
}

/// Matrix work requested by a mini-app (or a share), run on the worker.
#[derive(Debug)]
pub enum A2AppMatrixRequest {
    RoomInfo { room_id: OwnedRoomId, reply: Reply },
    ReadMessages { room_id: OwnedRoomId, limit: u32, reply: Reply },
    SendMessage { room_id: OwnedRoomId, body: String, reply: Reply },
    Profile { reply: Reply },
    Members { room_id: OwnedRoomId, limit: u32, reply: Reply },
    PinnedEvents { room_id: OwnedRoomId, reply: Reply },
    Threads { room_id: OwnedRoomId, limit: u32, reply: Reply },
    /// Start or stop the worker's watch on a room for the incoming hooks.
    WatchRoom { room_id: OwnedRoomId },
    UnwatchRoom { room_id: OwnedRoomId },
    /// Sends an app bundle into a room as an `rs.robius.a2app` event.
    /// No reply: outcome is reported via a popup notification.
    ShareApp { room_id: OwnedRoomId, bundle_json: String, app_name: String },
}

/// One isolate's live room hooks.
struct HookSubscription {
    app_id: MiniAppId,
    room_id: OwnedRoomId,
    hooks: HashSet<&'static str>,
}

/// What a mini-app asked the RoomScreen of its room to do.
#[derive(Clone, Debug)]
pub enum RoomAction {
    ShowUserProfile(OwnedUserId),
    JumpToEvent(OwnedEventId),
    ReplyTo(OwnedEventId),
    InsertDraft(String),
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

/// A finished matrix service call, posted back to the UI thread so the
/// result can re-enter the requesting isolate.
#[derive(Debug)]
pub struct A2AppMatrixResult {
    pub reply: Reply,
    pub result: Result<String, String>,
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
    if let Event::Actions(actions) = event {
        for action in actions {
            if let Some(watch_event) = action.downcast_ref::<A2AppRoomWatchEvent>() {
                watch_events.push(watch_event.clone());
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
    if !watch_events.is_empty() {
        deliver_room_hooks(cx, ui, watch_events);
    }

    advance_generation(cx, ui);
    process_broker(cx, ui);
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
/// and stops watching rooms nobody listens to any more.
fn prune_hook_subs() {
    with_a2app(|state| {
        let A2AppState { hook_subs, registry, permissions, watched_rooms, .. } = state;
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
        let wanted: HashSet<OwnedRoomId> = hook_subs.values().map(|s| s.room_id.clone()).collect();
        for room_id in watched_rooms.difference(&wanted) {
            submit_async_request(MatrixRequest::A2App(A2AppMatrixRequest::UnwatchRoom { room_id: room_id.clone() }));
        }
        for room_id in wanted.difference(watched_rooms) {
            submit_async_request(MatrixRequest::A2App(A2AppMatrixRequest::WatchRoom { room_id: room_id.clone() }));
        }
        *watched_rooms = wanted;
    });
}

/// Hands each watched room's news to the isolates subscribed to it.
fn deliver_room_hooks(cx: &mut Cx, ui: &WidgetRef, events: Vec<A2AppRoomWatchEvent>) {
    prune_hook_subs();
    // Messages batch into one call per pass; the other kinds coalesce to
    // their latest value.
    let mut messages: HashMap<OwnedRoomId, Vec<serde_json::Value>> = HashMap::new();
    let mut members: HashMap<OwnedRoomId, u64> = HashMap::new();
    let mut pins: HashMap<OwnedRoomId, Vec<OwnedEventId>> = HashMap::new();
    let mut closed: Vec<OwnedRoomId> = Vec::new();
    for event in events {
        match event.kind {
            RoomWatchKind::Message { event_id, sender, sender_name, body, msgtype, ts, is_own } => {
                messages.entry(event.room_id.clone()).or_default().push(serde_json::json!({
                    "room_id": event.room_id,
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
            RoomWatchKind::MembersChanged { count } => { members.insert(event.room_id, count); }
            RoomWatchKind::PinsChanged { pinned } => { pins.insert(event.room_id, pinned); }
            RoomWatchKind::Closed => closed.push(event.room_id),
        }
    }
    let subs: Vec<(usize, OwnedRoomId, HashSet<&'static str>)> = with_a2app(|state| {
        state.hook_subs.iter().map(|(heap, s)| (*heap, s.room_id.clone(), s.hooks.clone())).collect()
    }).unwrap_or_default();
    let mut delivered = false;
    for (heap, room_id, hooks) in subs {
        if hooks.contains("on_room_message")
            && let Some(batch) = messages.get(&room_id)
        {
            let payload = serde_json::Value::Array(batch.clone()).to_string();
            delivered |= instances::call_hook_by_heap(cx, heap, live_id!(on_room_message), &[&payload]);
        }
        if hooks.contains("on_room_members_changed")
            && let Some(count) = members.get(&room_id)
        {
            let payload = serde_json::json!({ "room_id": room_id, "count": count }).to_string();
            delivered |= instances::call_hook_by_heap(cx, heap, live_id!(on_room_members_changed), &[&payload]);
        }
        if hooks.contains("on_room_pins_changed")
            && let Some(pinned) = pins.get(&room_id)
        {
            let payload = serde_json::json!({ "room_id": room_id, "pinned": pinned }).to_string();
            delivered |= instances::call_hook_by_heap(cx, heap, live_id!(on_room_pins_changed), &[&payload]);
        }
    }
    if !closed.is_empty() {
        with_a2app(|state| {
            state.hook_subs.retain(|_, s| !closed.contains(&s.room_id));
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
        A2AppOp::RestoreVersion { app_id, stamp } => {
            let restored = with_a2app(|state| {
                let manifest = state.registry.get(&app_id).cloned()?;
                let source = persistence::load_version_source(&app_id, &stamp)?;
                let version = persistence::list_versions(&app_id).into_iter().find(|v| v.stamp == stamp)?;
                // Snapshot what's being replaced so the restore is undoable.
                let restore_label = a2app_core::versions::label_for(version.at_unix, utc_offset_secs());
                let _ = persistence::snapshot_version(&manifest, a2app_core::versions::version_of(
                    &manifest,
                    &format!("Before restoring {restore_label}"),
                    a2app_core::versions::now_unix(),
                    utc_offset_secs(),
                ));
                let mut updated = manifest;
                updated.source = source;
                updated.name = version.name.clone();
                updated.icon = version.icon.clone();
                updated.tint = version.tint;
                if let Err(e) = persistence::save_user_app(&updated) {
                    error!("Failed to save restored mini-app: {e}");
                }
                state.registry.insert(updated.clone());
                Some(updated)
            }).flatten();
            match restored {
                Some(updated) => {
                    // The restarted isolate boots the restored source.
                    restart_running_app(cx, ui, &updated);
                    enqueue_popup_notification(format!("Restored \"{}\".", updated.name), PopupKind::Success, Some(4.0));
                    ui.redraw(cx);
                }
                None => enqueue_popup_notification("Couldn't restore that version.", PopupKind::Error, Some(4.0)),
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
        A2AppOp::StartModify { app_id, request } => start_generation(cx, ui, request, None, Some(app_id)),
        A2AppOp::CancelGeneration => {
            with_a2app(|state| {
                // Dropping the Generation kills the agent child process.
                state.generation = None;
                state.console.status = String::from("Cancelled.");
            });
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
                if on { "Mini-apps may now write to rooms; each one still asks you first." }
                else { "Mini-apps can no longer write to rooms." },
                PopupKind::Info, Some(4.0),
            );
            ui.redraw(cx);
        }
        A2AppOp::ShareToRoom { app_id, room_id } => {
            if !with_a2app(|state| state.permissions.matrix_write()).unwrap_or(false) {
                enqueue_popup_notification(
                    format!("{MATRIX_WRITE_OFF_MSG}, so sharing an app into a room is blocked too."),
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
    modify: Option<MiniAppId>,
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

        // An explicit Modify wins; otherwise classify the request text.
        let refine_target = modify.or_else(|| {
            match a2app_agent::intent::classify(&request, &apps) {
                a2app_agent::intent::Intent::Modify(id) => Some(id),
                a2app_agent::intent::Intent::Create => None,
            }
        });

        let scope = match &room_id {
            Some(r) => A2AppScope::Room { room_id: r.to_string() },
            None => A2AppScope::Account,
        };
        state.create_room = room_id.clone();

        let generation = match refine_target.and_then(|id| state.registry.get(&id).cloned()) {
            Some(base) => {
                // Archive the current state first so the rewrite is undoable.
                let _ = persistence::snapshot_version(&base, a2app_core::versions::version_of(
                    &base,
                    &request,
                    a2app_core::versions::now_unix(),
                    utc_offset_secs(),
                ));
                Generation::start_refine(request.clone(), base, state.agent_prefs.clone())
            }
            None => Generation::start(request.clone(), taken, scope, state.agent_prefs.clone()),
        };
        match generation {
            Ok(generation) => {
                state.generation = Some(generation);
                state.console = GenConsole {
                    status: String::from("Connecting to the agent…"),
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
        Failed,
    }
    let done = with_a2app(|state| {
        let generation = state.generation.as_mut()?;
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
                Some(Done::Failed)
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
            // A rewritten app that's running restarts so the new source boots.
            restart_running_app(cx, ui, &manifest);
            enqueue_popup_notification(
                format!("Mini-app \"{}\" is ready.", manifest.name),
                PopupKind::Success, Some(4.0),
            );
            ui.redraw(cx);
        }
        Some(Done::Failed) => {
            with_a2app(|state| state.generation = None);
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
        broker.process(cx, BrokerCtx {
            registry,
            permissions,
            foreground_app: foreground_app.as_deref(),
            is_docked: &is_docked,
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
        BrokerAsk::Prompt { app_id, perm, request } => {
            queue_permission_prompt(cx, ui, app_id, perm, request);
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
            let _ = &app_id;
            let room = room.and_then(|r| OwnedRoomId::try_from(r.as_str()).ok());
            let request = match (call, room) {
                (MatrixServiceCall::Profile, _) => A2AppMatrixRequest::Profile { reply },
                (_, None) => {
                    services::respond(cx, reply, Err("this mini-app is not attached to a room"));
                    return;
                }
                (MatrixServiceCall::RoomInfo, Some(room_id)) =>
                    A2AppMatrixRequest::RoomInfo { room_id, reply },
                (MatrixServiceCall::ReadMessages { limit }, Some(room_id)) =>
                    A2AppMatrixRequest::ReadMessages { room_id, limit, reply },
                (MatrixServiceCall::SendMessage { body }, Some(room_id)) =>
                    A2AppMatrixRequest::SendMessage { room_id, body, reply },
                (MatrixServiceCall::Members { limit }, Some(room_id)) =>
                    A2AppMatrixRequest::Members { room_id, limit, reply },
                (MatrixServiceCall::PinnedEvents, Some(room_id)) =>
                    A2AppMatrixRequest::PinnedEvents { room_id, reply },
                (MatrixServiceCall::Threads { limit }, Some(room_id)) =>
                    A2AppMatrixRequest::Threads { room_id, limit, reply },
            };
            submit_async_request(MatrixRequest::A2App(request));
        }
        BrokerAsk::Subscribe { reply, app_id, heap_key, room, hook } => {
            let Ok(room_id) = OwnedRoomId::try_from(room.as_str()) else {
                return services::respond(cx, reply, Err("not a valid room id"));
            };
            with_a2app(|state| {
                let sub = state.hook_subs.entry(heap_key).or_insert_with(|| HookSubscription {
                    app_id,
                    room_id: room_id.clone(),
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
        BrokerAsk::HostAction { reply, app_id, action } => {
            // Only the modal's own isolate has to get out of the way when it
            // sends the user elsewhere; closing quits it, like its Close button.
            let leaves_modal = !matches!(action, HostAction::OpenApp { .. })
                && host_pane(cx, ui).active().is_some_and(|key| {
                    key.0 == app_id && instances::heap_of(&key) == Some(reply.heap_key)
                });
            match perform_host_action(cx, ui, action) {
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
    }
}

/// Applies one nav.* / composer.* call. Global moves happen right here;
/// room-scoped ones open the room and park the action for its RoomScreen.
fn perform_host_action(cx: &mut Cx, ui: &WidgetRef, action: HostAction) -> Result<(), String> {
    let room_of = |room: Option<String>| -> Result<OwnedRoomId, String> {
        let room = room.ok_or("this mini-app is not attached to a room; pass {room_id}")?;
        OwnedRoomId::try_from(room.as_str()).map_err(|_| String::from("not a valid room id"))
    };
    let event_of = |event_id: &str| {
        OwnedEventId::try_from(event_id).map_err(|_| String::from("not a valid event id"))
    };
    // Only rooms (and spaces) the user is in: anything else would open a
    // join dialog on the app's say-so.
    let joined_name = |cx: &mut Cx, room_id: &OwnedRoomId| -> Result<RoomNameId, String> {
        let rooms = cx.get_global::<RoomsListRef>();
        if rooms.get_room_state(room_id) != Some(RoomState::Joined) {
            return Err(String::from("not a room you're in"));
        }
        Ok(rooms.get_room_name(room_id).unwrap_or_else(|| RoomNameId::empty(room_id.clone())))
    };
    let queue_room_action = |cx: &mut Cx, room_id: OwnedRoomId, action: RoomAction| -> Result<(), String> {
        let destination_room = BasicRoomDetails::Name(joined_name(cx, &room_id)?);
        with_a2app(|state| {
            state.room_action = Some(PendingRoomAction {
                room_id: room_id.clone(),
                action,
                since: Instant::now(),
            });
        });
        cx.action(AppStateAction::NavigateToRoom { room_to_close: None, destination_room });
        cx.action(A2AppRoomAction::Pending { room_id });
        Ok(())
    };
    match action {
        HostAction::OpenRoom { room } => {
            let room_id = room_of(Some(room))?;
            let destination_room = BasicRoomDetails::Name(joined_name(cx, &room_id)?);
            cx.action(AppStateAction::NavigateToRoom { room_to_close: None, destination_room });
        }
        HostAction::OpenThread { room, event_id } => {
            let room_id = room_of(room)?;
            let thread_root_event_id = event_of(&event_id)?;
            let room_name_id = joined_name(cx, &room_id)?;
            cx.widget_action(
                ui.widget_uid(),
                RoomsListAction::Selected(SelectedRoom::Thread { room_name_id, thread_root_event_id }),
            );
        }
        HostAction::OpenSpace { space } => {
            let space_id = room_of(Some(space))?;
            let space_name_id = joined_name(cx, &space_id)?;
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
            return perform_host_action(cx, ui, action);
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
    }
    Ok(())
}

fn queue_permission_prompt(
    cx: &mut Cx,
    ui: &WidgetRef,
    app_id: MiniAppId,
    perm: Permission,
    request: Option<SplashHostRequest>,
) {
    with_a2app(|state| {
        // "Not Now" this session: refuse without re-asking, so a looping
        // script can't nag its way to an accidental Allow.
        if state.dismissed_prompts.contains(&(app_id.clone(), perm)) {
            if let Some(request) = request {
                Broker::respond_denied(cx, &request);
            }
            return;
        }
        // Merge into an already-active or queued prompt for the same pair.
        let same = |p: &PermissionPrompt| p.app_id == app_id && p.perm == perm;
        if let Some(active) = state.active_prompt.as_mut().filter(|p| same(p)) {
            active.parked.extend(request);
            return;
        }
        if let Some(queued) = state.prompts.iter_mut().find(|p| same(p)) {
            queued.parked.extend(request);
            return;
        }
        state.prompts.push_back(PermissionPrompt {
            app_id,
            perm,
            parked: request.into_iter().collect(),
        });
    });
    show_next_permission_prompt(cx, ui);
}

fn show_next_permission_prompt(cx: &mut Cx, ui: &WidgetRef) {
    let info = with_a2app(|state| {
        if state.active_prompt.is_some() {
            return None;
        }
        let prompt = state.prompts.pop_front()?;
        let (app_name, app_icon, reason) = state.registry.get(&prompt.app_id)
            .map(|m| (m.name.clone(), m.icon.clone(), m.reason_for(prompt.perm).map(str::to_string)))
            .unwrap_or_else(|| (prompt.app_id.clone(), String::new(), None));
        // Name the exact ability that asked, not just its group; a parked
        // subscribe names the hook it is for.
        let capability = prompt.parked.first().and_then(|r| {
            let hook_name = (r.service == "events.subscribe")
                .then(|| serde_json::from_str::<serde_json::Value>(&r.args_json).ok())
                .flatten()
                .and_then(|args| args["event"].as_str().map(str::to_string));
            match hook_name {
                Some(name) => a2app_core::capabilities::for_hook(&name),
                None => a2app_core::capabilities::for_service(&r.service),
            }
        }).map(|c| c.title.to_string());
        let info = PromptInfo {
            app_name,
            app_icon,
            perm: prompt.perm,
            reason,
            capability,
        };
        state.active_prompt = Some(prompt);
        Some(info)
    }).flatten();
    let Some(info) = info else { return };
    ui.mini_app_permission_prompt(cx, ids!(a2app_permission_modal.content)).show(cx, &info);
    ui.modal(cx, ids!(a2app_permission_modal)).open(cx);
}

fn answer_permission_prompt(cx: &mut Cx, ui: &WidgetRef, answer: PermissionPromptAction) {
    ui.modal(cx, ids!(a2app_permission_modal)).close(cx);
    let Some(Some(prompt)) = with_a2app(|state| state.active_prompt.take()) else { return };

    let granted = match answer {
        PermissionPromptAction::Allow => {
            with_a2app(|state| {
                state.permissions.set(&prompt.app_id, prompt.perm, GrantState::Granted);
                state.perms_dirty = true;
            });
            true
        }
        PermissionPromptAction::AllowOnce => {
            // Session-only: never touches disk, dropped on isolate teardown.
            with_a2app(|state| state.permissions.grant_once(&prompt.app_id, prompt.perm));
            true
        }
        PermissionPromptAction::Deny => {
            with_a2app(|state| {
                state.permissions.set(&prompt.app_id, prompt.perm, GrantState::Denied);
                state.perms_dirty = true;
            });
            false
        }
        PermissionPromptAction::NotNow => {
            with_a2app(|state| {
                state.dismissed_prompts.insert((prompt.app_id.clone(), prompt.perm));
            });
            false
        }
        PermissionPromptAction::None => {
            // Shouldn't happen; put the prompt back.
            with_a2app(|state| state.active_prompt = Some(prompt));
            return;
        }
    };
    publish_grants(cx);

    // Replay or refuse everything parked behind this prompt.
    for request in prompt.parked {
        if granted {
            let asks = with_a2app(|state| {
                let A2AppState { broker, registry, permissions, foreground_app, .. } = state;
                let is_docked = |app_id: &str| instances::is_docked(app_id);
                broker.dispatch_after_grant(cx, BrokerCtx {
                    registry,
                    permissions,
                    foreground_app: foreground_app.as_deref(),
                    is_docked: &is_docked,
                }, request)
            }).unwrap_or_default();
            for ask in asks {
                apply_broker_ask(cx, ui, ask);
            }
        } else {
            Broker::respond_denied(cx, &request);
        }
    }

    apply_permission_to_running(cx, ui, &prompt.app_id, prompt.perm);
    show_next_permission_prompt(cx, ui);
    ui.redraw(cx);
}

/// Pushes a changed grant into the app's live isolate: network changes
/// restart the app (the net runtime is baked in at VM alloc); anything else
/// just gets the new caps list plus an `on_permissions_changed` call.
fn apply_permission_to_running(cx: &mut Cx, ui: &WidgetRef, app_id: &str, perm: Permission) {
    prune_hook_subs();
    if !with_a2app(|state| state.is_running(app_id)).unwrap_or(false) {
        return;
    }
    let grants = a2app_core::permissions::snapshot_grants_for(app_id);
    if perm == Permission::Network {
        let Some(Some(manifest)) = with_a2app(|state| state.registry.get(app_id).cloned()) else { return };
        restart_running_app(cx, ui, &manifest);
    } else {
        instances::update_app_caps(cx, app_id, grants);
    }
}

/// Restarts a running app's isolate on whichever surface hosts it, so the
/// current source and grants take effect (the net runtime especially, which
/// is baked in at VM alloc).
fn restart_running_app(cx: &mut Cx, ui: &WidgetRef, manifest: &MiniAppManifest) {
    let grants = a2app_core::permissions::snapshot_grants_for(&manifest.id);
    let keys: Vec<InstanceKey> = instances::keys_of_app(&manifest.id);
    for key in &keys {
        instances::restart(cx, key, manifest, &grants);
    }
    prune_hook_subs();
    if keys.is_empty() {
        return;
    }
    host_pane(cx, ui).refresh_host(cx);
    cx.action(DockCmd::Restart(manifest.id.clone()));
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

/// Cuts a body to `max` chars on a char boundary; a byte truncate could
/// split a multi-byte char and panic.
fn clip_chars(s: &mut String, max: usize) {
    if let Some((idx, _)) = s.char_indices().nth(max) {
        s.truncate(idx);
    }
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
// Matrix worker-side handlers
// -----------------------------------------------------------------------

/// Runs one mini-app matrix operation on the worker's async runtime and
/// posts the result back to the UI thread.
pub async fn handle_matrix_request(request: A2AppMatrixRequest) {
    use crate::sliding_sync::{current_user_id, get_client};

    let (reply, result) = match request {
        A2AppMatrixRequest::RoomInfo { room_id, reply } => {
            let result: Result<String, String> = async {
                let client = get_client().ok_or("not logged in")?;
                let room = client.get_room(&room_id).ok_or("room not found")?;
                let room_name = room.display_name().await
                    .map(|n| n.to_string())
                    .unwrap_or_else(|_| room_id.to_string());
                let join_rule = room.join_rule()
                    .map(|r| r.as_str().to_string())
                    .unwrap_or_else(|| String::from("unknown"));
                let history = room.history_visibility_or_default().as_str().to_string();
                let body = serde_json::json!({
                    "room_id": room_id.to_string(),
                    "room_name": room_name,
                    "topic": room.topic().unwrap_or_default(),
                    "member_count": room.active_members_count(),
                    "encrypted": room.encryption_state().is_encrypted(),
                    "join_rule": join_rule,
                    "history_visibility": history,
                });
                Ok(body.to_string())
            }.await;
            (reply, result)
        }
        A2AppMatrixRequest::ReadMessages { room_id, limit, reply } => {
            let result: Result<String, String> = async {
                use matrix_sdk::room::MessagesOptions;
                use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent};
                let client = get_client().ok_or("not logged in")?;
                let room = client.get_room(&room_id).ok_or("room not found")?;
                let mut out: Vec<serde_json::Value> = Vec::new();
                // The event cache already holds the recent timeline in
                // memory; only hit the network when it can't fill the request.
                if let Ok((cache, _guard)) = client.event_cache().room(&room_id).await {
                    if let Ok(events) = cache.events().await {
                        for event in events.iter().rev() {
                            let Ok(AnySyncTimelineEvent::MessageLike(
                                AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(msg))
                            )) = event.raw().deserialize() else { continue };
                            let mut body = msg.content.body().to_string();
                            clip_chars(&mut body, 500);
                            out.push(serde_json::json!({
                                "sender": msg.sender.localpart(),
                                "sender_id": msg.sender,
                                "event_id": msg.event_id,
                                "body": body,
                            }));
                            if out.len() >= limit as usize {
                                break;
                            }
                        }
                    }
                }
                if out.len() < limit as usize {
                    // A room's recent tail can be all state events (profile
                    // changes etc), so keep paginating until we fill `limit`.
                    out.clear();
                    let mut from: Option<String> = None;
                    for _ in 0..4 {
                        let mut options = MessagesOptions::backward();
                        options.limit = 50u32.into();
                        options.from = from;
                        let messages = room.messages(options).await
                            .map_err(|e| format!("couldn't read messages: {e}"))?;
                        for event in messages.chunk {
                            let Ok(AnySyncTimelineEvent::MessageLike(
                                AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(msg))
                            )) = event.raw().deserialize() else { continue };
                            let mut body = msg.content.body().to_string();
                            clip_chars(&mut body, 500);
                            out.push(serde_json::json!({
                                "sender": msg.sender.localpart(),
                                "sender_id": msg.sender,
                                "event_id": msg.event_id,
                                "body": body,
                            }));
                            if out.len() >= limit as usize {
                                break;
                            }
                        }
                        from = messages.end;
                        if out.len() >= limit as usize || from.is_none() {
                            break;
                        }
                    }
                }
                // Backward pagination is newest-first; apps read oldest-first.
                out.reverse();
                Ok(serde_json::json!({ "messages": out }).to_string())
            }.await;
            (reply, result)
        }
        A2AppMatrixRequest::SendMessage { room_id, body, reply } => {
            let result: Result<String, String> = async {
                use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
                let client = get_client().ok_or("not logged in")?;
                let room = client.get_room(&room_id).ok_or("room not found")?;
                room.send(RoomMessageEventContent::text_plain(body)).await
                    .map_err(|e| format!("couldn't send the message: {e}"))?;
                Ok(String::from("{}"))
            }.await;
            (reply, result)
        }
        A2AppMatrixRequest::Members { room_id, limit, reply } => {
            let result: Result<String, String> = async {
                use matrix_sdk::RoomMemberships;
                let client = get_client().ok_or("not logged in")?;
                let room = client.get_room(&room_id).ok_or("room not found")?;
                // A full /members sync on a big room can take tens of
                // seconds, so serve from the store unless it's too sparse
                // to even fill the requested page.
                let joined_count = room.joined_members_count();
                let mut members = room.members_no_sync(RoomMemberships::JOIN).await
                    .unwrap_or_default();
                if (members.len() as u64) < joined_count.min(limit as u64) {
                    members = room.members(RoomMemberships::JOIN).await
                        .map_err(|e| format!("couldn't load members: {e}"))?;
                }
                let count = (members.len() as u64).max(joined_count);
                let out: Vec<serde_json::Value> = members.iter()
                    .take(limit as usize)
                    .map(|m| {
                        use matrix_sdk::ruma::events::room::power_levels::UserPowerLevel;
                        // A room creator's power is "infinite" from room v12 on.
                        let power: i64 = match m.power_level() {
                            UserPowerLevel::Int(int) => int.into(),
                            _ => i64::MAX,
                        };
                        serde_json::json!({
                            "name": m.name(),
                            "user_id": m.user_id().to_string(),
                            "power": power,
                        })
                    })
                    .collect();
                Ok(serde_json::json!({ "count": count, "members": out }).to_string())
            }.await;
            (reply, result)
        }
        A2AppMatrixRequest::PinnedEvents { room_id, reply } => {
            let result: Result<String, String> = async {
                use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent};
                let client = get_client().ok_or("not logged in")?;
                let room = client.get_room(&room_id).ok_or("room not found")?;
                let pinned_ids = room.pinned_event_ids().unwrap_or_default();
                let mut out: Vec<serde_json::Value> = Vec::new();
                // Serve each pin from the event cache/store; only misses go
                // to the network, and those run concurrently.
                let cache = client.event_cache().room(&room_id).await.ok();
                let cache_ref = cache.as_ref().map(|(c, _)| c);
                let room_ref = &room;
                let fetched = futures_util::future::join_all(
                    pinned_ids.iter().take(10).map(|event_id| async move {
                        if let Some(c) = cache_ref {
                            if let Ok(Some(event)) = c.find_event(event_id).await {
                                return Some(event);
                            }
                        }
                        room_ref.event(event_id, None).await.ok()
                    })
                ).await;
                for event in fetched.into_iter().flatten() {
                    let Ok(AnySyncTimelineEvent::MessageLike(
                        AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(msg))
                    )) = event.raw().deserialize() else { continue };
                    let mut body = msg.content.body().to_string();
                    clip_chars(&mut body, 300);
                    out.push(serde_json::json!({
                        "sender": msg.sender.localpart(),
                        "sender_id": msg.sender,
                        "event_id": msg.event_id,
                        "body": body,
                    }));
                }
                Ok(serde_json::json!({ "pinned": out }).to_string())
            }.await;
            (reply, result)
        }
        A2AppMatrixRequest::Threads { room_id, limit, reply } => {
            let result: Result<String, String> = async {
                use matrix_sdk::room::ListThreadsOptions;
                use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent};
                let client = get_client().ok_or("not logged in")?;
                let room = client.get_room(&room_id).ok_or("room not found")?;
                let opts = ListThreadsOptions::default();
                let roots = room.list_threads(opts).await
                    .map_err(|e| format!("couldn't list threads: {e}"))?;
                let mut out: Vec<serde_json::Value> = Vec::new();
                for event in roots.chunk.iter().take(limit as usize) {
                    let Ok(AnySyncTimelineEvent::MessageLike(
                        AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(msg))
                    )) = event.raw().deserialize() else { continue };
                    let mut body = msg.content.body().to_string();
                    clip_chars(&mut body, 300);
                    out.push(serde_json::json!({
                        "sender": msg.sender.localpart(),
                        "sender_id": msg.sender,
                        "event_id": msg.event_id,
                        "body": body,
                    }));
                }
                Ok(serde_json::json!({ "threads": out }).to_string())
            }.await;
            (reply, result)
        }
        A2AppMatrixRequest::WatchRoom { room_id } => {
            room_watch::start_watch(room_id);
            return;
        }
        A2AppMatrixRequest::UnwatchRoom { room_id } => {
            room_watch::stop_watch(&room_id);
            return;
        }
        A2AppMatrixRequest::Profile { reply } => {
            let result: Result<String, String> = async {
                let client = get_client().ok_or("not logged in")?;
                let user_id = current_user_id().ok_or("not logged in")?;
                let display_name = client.account().get_display_name().await
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| user_id.localpart().to_string());
                Ok(serde_json::json!({
                    "user_id": user_id.to_string(),
                    "display_name": display_name,
                }).to_string())
            }.await;
            (reply, result)
        }
        A2AppMatrixRequest::ShareApp { room_id, bundle_json, app_name } => {
            let result: Result<(), String> = async {
                let client = get_client().ok_or("not logged in")?;
                let room = client.get_room(&room_id).ok_or("room not found")?;
                let content = serde_json::json!({
                    "body": format!("Shared a Splash mini-app: {app_name}"),
                    "bundle": bundle_json,
                });
                let raw = serde_json::value::to_raw_value(&content)
                    .map_err(|e| e.to_string())?;
                room.send_raw(crate::a2app::timeline_card::A2APP_EVENT_TYPE, raw).await
                    .map_err(|e| format!("couldn't share the app: {e}"))?;
                Ok(())
            }.await;
            match result {
                Ok(()) => enqueue_popup_notification(
                    format!("Shared \"{app_name}\" into the room."),
                    PopupKind::Success, Some(4.0),
                ),
                Err(e) => enqueue_popup_notification(e, PopupKind::Error, Some(5.0)),
            }
            return;
        }
    };
    Cx::post_action(A2AppMatrixResult { reply, result });
    SignalToUI::set_ui_signal();
}
