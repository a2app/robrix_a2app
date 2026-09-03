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
use a2app_core::services::{self, Broker, BrokerAsk, BrokerCtx, HostAction, HostQuery, Reply, MATRIX_WRITE_OFF_MSG};
use a2app_core::versions::{self, VersionOrigin};
use a2app_agent::intent::Intent;
use a2app_agent::pipeline::{GenOutcome, Generation};
use a2app_agent::prefs::AgentPrefs;

use crate::a2app::host_pane::{MiniAppHostPaneAction, MiniAppHostPaneWidgetRefExt};
use crate::a2app::permission_prompt::{
    MiniAppPermissionPromptWidgetRefExt, PermissionPromptAction, PromptInfo,
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
    /// Whether the worker runs the account watch for them too.
    account_watched: bool,
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

    advance_generation(cx, ui);
    process_broker(cx, ui);

    // An action the user took that was refused or failed gets a popup that
    // says why, on top of the script's own error handling.
    for (app_id, error) in with_a2app(|state| state.broker.failed_acts()).unwrap_or_default() {
        let name = with_a2app(|state| state.registry.get(&app_id).map(|a| a.name.clone()))
            .flatten()
            .unwrap_or(app_id);
        enqueue_popup_notification(format!("{name}: {error}"), PopupKind::Warning, Some(7.0));
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
            let was_running = stop_for_restart(cx, ui, &manifest);
            enqueue_popup_notification(
                reopen_hint(format!("Mini-app \"{}\" is ready.", manifest.name), was_running),
                PopupKind::Success, Some(5.0),
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
        BrokerAsk::Matrix { reply, app_id: _, room, call } => {
            let room = room.and_then(|r| OwnedRoomId::try_from(r.as_str()).ok());
            match matrix::request_for(call, room, reply) {
                Ok(request) => submit_async_request(MatrixRequest::A2App(request)),
                Err(e) => services::respond(cx, reply, Err(e)),
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
    }
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
                (HostAction::SetSide { side }, Some(Surface::Dock)) => {
                    if side.is_vertical() && !desktop {
                        return Err(String::from("side panes need the desktop layout"));
                    }
                    PaneOp::SetSide(*side)
                }
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
        } else {
            Broker::respond_denied(cx, &request);
        }
    }

    apply_permission_to_running(cx, ui, &prompt.app_id, prompt.perm);
    show_next_permission_prompt(cx, ui);
    ui.redraw(cx);
}

/// Pushes a changed grant into the app's live isolate: network changes
/// stop the app (the net runtime is baked in at VM alloc); anything else
/// just gets the new caps list plus an `on_permissions_changed` call.
fn apply_permission_to_running(cx: &mut Cx, ui: &WidgetRef, app_id: &str, perm: Permission) {
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
