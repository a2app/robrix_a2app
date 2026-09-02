//! The registry of live mini-app instances: one isolate per (app, room),
//! owned here so a pane, tab or modal only ever borrows one to draw it.
//!
//! Surfaces `adopt` an instance to show it and `release` it when they go
//! away; the isolate, its script state and its pane layout survive room
//! switches, tab closes and view-mode changes. Only the explicit quit paths
//! tear an instance down. Parked hosts stay linked under the widget-tree
//! root so their scripts' `ui.*` calls keep resolving.

use std::cell::RefCell;
use std::collections::HashMap;

use makepad_widgets::*;
use makepad_widgets::widget_async::gc_dead_splash_isolates;
use matrix_sdk::ruma::{OwnedRoomId, RoomId};

use a2app_core::layout::PaneLayout;
use a2app_core::manifest::{instance_tag, MiniAppId, MiniAppManifest};
use a2app_core::services::PaneState;

/// `(app, room)`; `None` is a room-less app in the host modal.
pub type InstanceKey = (MiniAppId, Option<OwnedRoomId>);

/// What kind of surface shows an instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    Dock,
    Tab,
    Modal,
}

impl Surface {
    pub fn as_str(self) -> &'static str {
        match self {
            Surface::Dock => "dock",
            Surface::Tab => "tab",
            Surface::Modal => "modal",
        }
    }
}

struct MiniAppInstance {
    host: WidgetRef,
    heap_key: Option<usize>,
    /// Content size the script was last told about.
    last_size: Vec2d,
    pending_resize: Option<Vec2d>,
    layout: PaneLayout,
    /// The surface drawing it; `None` while parked.
    shown_by: Option<WidgetUid>,
    surface: Option<Surface>,
    /// False after its surface died without a `Cx` to re-anchor it.
    anchored: bool,
}

impl MiniAppInstance {
    fn foreground(&self) -> bool {
        match self.surface {
            Some(Surface::Dock) => !self.layout.minimized,
            Some(_) => true,
            None => false,
        }
    }
}

#[derive(Default)]
struct Registry {
    host_template: Option<ScriptObjectRef>,
    instances: HashMap<InstanceKey, MiniAppInstance>,
    needs_anchor: bool,
    has_pending_resize: bool,
    /// Surface and focus hooks owed to instances; payloads are built at
    /// flush time, so a burst of changes is one call with the final state.
    pending_hooks: Vec<(InstanceKey, LiveId)>,
}

thread_local! {
    static INSTANCES: RefCell<Registry> = RefCell::new(Registry::default());
}

fn with_registry<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
    INSTANCES.with(|r| f(&mut r.borrow_mut()))
}

/// Emitted when an app's last instance is gone, so the runtime can drop
/// its one-time grants.
#[derive(Clone, Debug, Default)]
pub enum MiniAppInstanceAction {
    AppStopped(MiniAppId),
    #[default]
    None,
}

pub fn tag_of(key: &InstanceKey) -> String {
    instance_tag(&key.0, key.1.as_deref().map(RoomId::as_str))
}

fn tree_name(key: &InstanceKey) -> LiveId {
    LiveId::from_str(&format!("a2app:{}", tag_of(key)))
}

/// The `AppHost` template every spawn instantiates; any host surface's
/// `on_after_apply` may register it.
pub fn set_host_template(obj: ScriptObjectRef) {
    with_registry(|r| r.host_template = Some(obj));
}

pub fn clear_host_template() {
    with_registry(|r| r.host_template = None);
}

fn splash_of(cx: &mut Cx, host: &WidgetRef) -> WidgetRef {
    host.widget(cx, ids!(splash))
}

/// Instantiates a host and evals the app into a fresh isolate, parked
/// under the tree root. All isolate config lands BEFORE the source evals.
fn spawn(cx: &mut Cx, manifest: &MiniAppManifest, grants: &[String], key: &InstanceKey) -> Option<(WidgetRef, Option<usize>)> {
    let Some(template) = with_registry(|r| r.host_template.clone()) else {
        error!("BUG: no mini-app host template registered");
        return None;
    };
    let template_value: ScriptValue = template.as_object().into();
    let host = cx.with_vm(|vm| WidgetRef::script_from_value(vm, template_value));
    host.set_visible(cx, true);
    let root = cx.widget_tree().root_uid();
    cx.widget_tree_insert_child_deep(root, tree_name(key), host.clone());

    let tag = tag_of(key);
    if let Some(mut splash) = splash_of(cx, &host).borrow_mut::<Splash>() {
        splash.set_allow_net(grants.iter().any(|g| g == "network"));
        splash.set_sandbox_dir(cx, Some(a2app_core::app_sandbox_dir(&manifest.id)));
        splash.set_host_tag(cx, Some(tag));
        splash.set_host_caps(cx, grants.to_vec());
        splash.set_host_prompts(cx, true);
        splash.set_debug_name(&manifest.id);
    }
    splash_of(cx, &host).set_text(cx, &manifest.source);
    let heap_key = splash_of(cx, &host).borrow_mut::<Splash>()
        .and_then(|mut splash| splash.isolate_heap_key(cx));
    Some((host, heap_key))
}

/// The host for `key`, creating its isolate if it isn't running yet.
/// `seed` is the layout a brand-new instance starts with.
pub fn ensure(
    cx: &mut Cx,
    key: &InstanceKey,
    manifest: &MiniAppManifest,
    grants: &[String],
    seed: PaneLayout,
) -> Option<WidgetRef> {
    if let Some(host) = host_of(key) {
        return Some(host);
    }
    let (host, heap_key) = spawn(cx, manifest, grants, key)?;
    with_registry(|r| {
        r.instances.insert(key.clone(), MiniAppInstance {
            host: host.clone(),
            heap_key,
            last_size: Vec2d::default(),
            pending_resize: None,
            layout: seed,
            shown_by: None,
            surface: None,
            anchored: true,
        });
    });
    Some(host)
}

/// Claims the instance for `surface_uid` to draw. `None` if it doesn't
/// exist or another surface is already showing it.
pub fn adopt(cx: &mut Cx, key: &InstanceKey, surface_uid: WidgetUid, surface: Surface) -> Option<WidgetRef> {
    let host = with_registry(|r| {
        let inst = r.instances.get_mut(key)?;
        if inst.shown_by.is_some_and(|uid| uid != surface_uid) {
            return None;
        }
        inst.shown_by = Some(surface_uid);
        inst.surface = Some(surface);
        inst.anchored = true;
        Some(inst.host.clone())
    })?;
    cx.widget_tree_insert_child_deep(surface_uid, tree_name(key), host.clone());
    note_hook(key, live_id!(on_surface_changed));
    note_hook(key, live_id!(on_focus_changed));
    Some(host)
}

/// Parks the instance again; a no-op unless `surface_uid` is showing it.
pub fn release(cx: &mut Cx, key: &InstanceKey, surface_uid: WidgetUid) {
    let host = with_registry(|r| {
        let inst = r.instances.get_mut(key)?;
        if inst.shown_by != Some(surface_uid) {
            return None;
        }
        inst.shown_by = None;
        inst.surface = None;
        inst.anchored = true;
        Some(inst.host.clone())
    });
    if let Some(host) = host {
        let root = cx.widget_tree().root_uid();
        cx.widget_tree_insert_child_deep(root, tree_name(key), host);
        note_hook(key, live_id!(on_focus_changed));
    }
}

/// For a surface's `Drop`: parks everything it showed; re-anchoring waits
/// for the next event pass, which has a `Cx`.
pub fn release_owner_no_cx(surface_uid: WidgetUid) {
    with_registry(|r| {
        for (key, inst) in r.instances.iter_mut() {
            if inst.shown_by == Some(surface_uid) {
                inst.shown_by = None;
                inst.surface = None;
                inst.anchored = false;
                r.needs_anchor = true;
                let owed = (key.clone(), live_id!(on_focus_changed));
                if !r.pending_hooks.contains(&owed) {
                    r.pending_hooks.push(owed);
                }
            }
        }
    });
}

/// Owes the instance one `on_surface_changed` / `on_focus_changed` call,
/// delivered by `flush_pending` with the state at that time.
pub fn note_hook(key: &InstanceKey, hook: LiveId) {
    with_registry(|r| {
        if !r.instances.contains_key(key) {
            return;
        }
        let owed = (key.clone(), hook);
        if !r.pending_hooks.contains(&owed) {
            r.pending_hooks.push(owed);
        }
    });
}

pub fn surface_of(key: &InstanceKey) -> Option<Surface> {
    with_registry(|r| r.instances.get(key).and_then(|i| i.surface))
}

/// The pane of the isolate with `heap_key`, for `env` and `ui.pane.read`.
pub fn pane_state(heap_key: usize) -> Option<PaneState> {
    with_registry(|r| {
        let inst = r.instances.values().find(|i| i.heap_key == Some(heap_key))?;
        Some(PaneState {
            surface: inst.surface.map_or("parked", Surface::as_str),
            side: (inst.surface == Some(Surface::Dock)).then_some(inst.layout.side),
            minimized: inst.surface == Some(Surface::Dock) && inst.layout.minimized,
            foreground: inst.foreground(),
            width: inst.last_size.x,
            height: inst.last_size.y,
        })
    })
}

pub fn shown_by(key: &InstanceKey) -> Option<WidgetUid> {
    with_registry(|r| r.instances.get(key).and_then(|i| i.shown_by))
}

pub fn host_of(key: &InstanceKey) -> Option<WidgetRef> {
    with_registry(|r| r.instances.get(key).map(|i| i.host.clone()))
}

pub fn heap_of(key: &InstanceKey) -> Option<usize> {
    with_registry(|r| r.instances.get(key).and_then(|i| i.heap_key))
}

pub fn key_of_heap(heap_key: usize) -> Option<InstanceKey> {
    with_registry(|r| {
        r.instances.iter()
            .find(|(_, i)| i.heap_key == Some(heap_key))
            .map(|(k, _)| k.clone())
    })
}

pub fn layout(key: &InstanceKey) -> PaneLayout {
    with_registry(|r| r.instances.get(key).map(|i| i.layout)).unwrap_or_default()
}

pub fn set_layout(key: &InstanceKey, layout: PaneLayout) {
    with_registry(|r| {
        if let Some(inst) = r.instances.get_mut(key) {
            inst.layout = layout;
        }
    });
}

/// Apps with an instance in `room`, sorted so adoption order is stable.
pub fn apps_in_room(room: &RoomId) -> Vec<MiniAppId> {
    let mut apps: Vec<MiniAppId> = with_registry(|r| {
        r.instances.keys()
            .filter(|(_, r)| r.as_deref() == Some(room))
            .map(|(app, _)| app.clone())
            .collect()
    });
    apps.sort();
    apps
}

pub fn keys_of_app(app_id: &str) -> Vec<InstanceKey> {
    with_registry(|r| r.instances.keys().filter(|(app, _)| app == app_id).cloned().collect())
}

pub fn is_running(app_id: &str) -> bool {
    with_registry(|r| r.instances.keys().any(|(app, _)| app == app_id))
}

/// Whether any instance of the app is bound to a room (docked, in a tab,
/// or popped out of one).
pub fn is_docked(app_id: &str) -> bool {
    with_registry(|r| r.instances.keys().any(|(app, room)| app == app_id && room.is_some()))
}

/// Drops the instance and reclaims its isolate. Returns true when that was
/// the app's last instance.
pub fn quit(cx: &mut Cx, key: &InstanceKey) -> bool {
    let removed = with_registry(|r| r.instances.remove(key).is_some());
    if !removed {
        return false;
    }
    gc(cx);
    !is_running(&key.0)
}

/// Drops every instance of the app. Returns whether any existed.
pub fn quit_app(cx: &mut Cx, app_id: &str) -> bool {
    let removed = with_registry(|r| {
        let before = r.instances.len();
        r.instances.retain(|(app, _), _| app != app_id);
        before != r.instances.len()
    });
    if removed {
        gc(cx);
    }
    removed
}

pub fn quit_everything(cx: &mut Cx) {
    with_registry(|r| r.instances.clear());
    gc(cx);
}

/// Reclaims isolates whose last reference just dropped; surfaces call it
/// after letting go of their own host clones.
pub fn gc(cx: &mut Cx) {
    gc_dead_splash_isolates(cx);
}

/// Queues an `on_app_resize` if the instance's content box changed.
pub fn note_size(key: &InstanceKey, size: Vec2d) {
    if size.x < 1.0 || size.y < 1.0 {
        return;
    }
    with_registry(|r| {
        let Some(inst) = r.instances.get_mut(key) else { return };
        let changed = (inst.last_size.x - size.x).abs() > 0.5 || (inst.last_size.y - size.y).abs() > 0.5;
        if changed {
            inst.last_size = size;
            inst.pending_resize = Some(size);
            r.has_pending_resize = true;
        }
    });
}

/// Event-time housekeeping: re-anchors hosts whose surface died and
/// delivers queued `on_app_resize` calls.
pub fn flush_pending(cx: &mut Cx) {
    let (to_anchor, resizes, hooks) = with_registry(|r| {
        if !r.needs_anchor && !r.has_pending_resize && r.pending_hooks.is_empty() {
            return (Vec::new(), Vec::new(), Vec::new());
        }
        r.needs_anchor = false;
        r.has_pending_resize = false;
        let mut to_anchor = Vec::new();
        let mut resizes = Vec::new();
        for (key, inst) in r.instances.iter_mut() {
            if !inst.anchored {
                inst.anchored = true;
                to_anchor.push((key.clone(), inst.host.clone()));
            }
            if let Some(size) = inst.pending_resize.take() {
                resizes.push((inst.host.clone(), size));
            }
        }
        let hooks: Vec<(WidgetRef, LiveId, String)> = std::mem::take(&mut r.pending_hooks).into_iter()
            .filter_map(|(key, hook)| {
                let inst = r.instances.get(&key)?;
                let payload = if hook == live_id!(on_focus_changed) {
                    serde_json::json!({ "foreground": inst.foreground() })
                } else {
                    serde_json::json!({
                        "surface": inst.surface.map_or("parked", Surface::as_str),
                        "side": (inst.surface == Some(Surface::Dock)).then(|| inst.layout.side.as_str()),
                    })
                };
                Some((inst.host.clone(), hook, payload.to_string()))
            })
            .collect();
        (to_anchor, resizes, hooks)
    });
    if !to_anchor.is_empty() {
        let root = cx.widget_tree().root_uid();
        for (key, host) in to_anchor {
            cx.widget_tree_insert_child_deep(root, tree_name(&key), host);
        }
    }
    for (host, size) in resizes {
        if let Some(mut splash) = splash_of(cx, &host).borrow_mut::<Splash>() {
            splash.call_script_fn(cx, live_id!(on_app_resize), &[size.x.into(), size.y.into()]);
        }
    }
    for (host, hook, payload) in hooks {
        if let Some(mut splash) = splash_of(cx, &host).borrow_mut::<Splash>() {
            splash.call_script_fn_with_strings(cx, hook, &[&payload]);
        }
    }
}

fn all_hosts() -> Vec<WidgetRef> {
    with_registry(|r| r.instances.values().map(|i| i.host.clone()).collect())
}

/// Network responses go to every isolate, shown or parked, so in-flight
/// requests complete.
pub fn handle_network_responses(cx: &mut Cx, event: &Event, scope: &mut Scope) {
    for host in all_hosts() {
        host.handle_event(cx, event, scope);
    }
}

/// Pushes a new caps list into every instance of the app and invokes its
/// optional `on_permissions_changed(caps)` hook.
pub fn update_app_caps(cx: &mut Cx, app_id: &str, grants: Vec<String>) {
    let caps_json = serde_json::to_string(&grants).unwrap_or_else(|_| String::from("[]"));
    let hosts: Vec<WidgetRef> = with_registry(|r| {
        r.instances.iter()
            .filter(|((app, _), _)| app == app_id)
            .map(|(_, i)| i.host.clone())
            .collect()
    });
    for host in hosts {
        if let Some(mut splash) = splash_of(cx, &host).borrow_mut::<Splash>() {
            splash.set_host_caps(cx, grants.clone());
            splash.call_script_fn_with_strings(cx, live_id!(on_permissions_changed), &[&caps_json]);
        }
    }
}

/// Delivers an IPC message to every instance of `to` except the sender's
/// own isolate. Returns whether anything received it.
pub fn deliver_ipc(cx: &mut Cx, from_heap: usize, from: &str, to: &str, data_json: &str) -> bool {
    let hosts: Vec<WidgetRef> = with_registry(|r| {
        r.instances.iter()
            .filter(|((app, _), i)| app == to && i.heap_key != Some(from_heap))
            .map(|(_, i)| i.host.clone())
            .collect()
    });
    let mut delivered = false;
    for host in hosts {
        if let Some(mut splash) = splash_of(cx, &host).borrow_mut::<Splash>() {
            delivered |= splash.call_script_fn_with_strings(cx, live_id!(on_ipc_message), &[from, data_json]);
        }
    }
    delivered
}

/// Calls an optional top-level script hook with string arguments.
/// Returns whether the script defined it.
pub fn call_hook(cx: &mut Cx, key: &InstanceKey, hook: LiveId, args: &[&str]) -> bool {
    let Some(host) = host_of(key) else { return false };
    splash_of(cx, &host).borrow_mut::<Splash>()
        .is_some_and(|mut splash| splash.call_script_fn_with_strings(cx, hook, args))
}

/// Like [`call_hook`], addressed by the isolate's heap key.
pub fn call_hook_by_heap(cx: &mut Cx, heap_key: usize, hook: LiveId, args: &[&str]) -> bool {
    let Some(key) = key_of_heap(heap_key) else { return false };
    call_hook(cx, &key, hook, args)
}
