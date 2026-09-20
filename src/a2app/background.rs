//! User-enabled jobs run in the ordinary mini-app isolate and permission jail.
//!
//! Scheduling is awake-process only. A single adaptive timer wakes for the
//! next due job or deadline; room conditions consume the live room watch.

use std::{cell::RefCell, collections::{BTreeMap, BTreeSet}, time::{Instant, SystemTime, UNIX_EPOCH}};
use a2app_core::{background::{self as jobs, Job, JobBinding, JobContext, JobRun, JobStore, PauseReason, RunOutcome, Trigger},
    information_flow::{self as flow, ContextId}, manifest::MiniAppManifest, permissions::{Effective, PermissionContext, RoomAccess, PolicyDecision}};
use makepad_widgets::*;
use matrix_sdk::{RoomState, ruma::OwnedRoomId};
use super::{instances::{self, InstanceKey}, runtime::with_a2app};

const DISPATCH_BATCH: usize = 4;
const RECONCILE_RETRY_MS: u64 = 30_000;
const CLOCK_RECHECK_MS: u64 = 60_000;

#[derive(Clone)]
pub struct TaskView {
    pub job: Job,
    pub current_fingerprint: Option<String>,
    pub status: String,
}

struct Activation {
    key: InstanceKey,
    heap: usize,
    context: ContextId,
    epoch: u64,
    run_id: u64,
    started: Instant,
}

#[derive(Default)]
struct Scheduler {
    store: Option<JobStore>,
    error: Option<String>,
    account: Option<String>,
    signed_out: bool,
    suspended: bool,
    retry_at: Option<u64>,
    timer: Option<Timer>,
    timer_at: Option<u64>,
    revision: u64,
    dirty: bool,
    active: BTreeMap<u64, Activation>,
    status: BTreeMap<u64, String>,
    failures: BTreeMap<u64, (usize, u64, String)>,
    last_owners: BTreeMap<u64, (usize, u64, ContextId)>,
    stopped_on_error: bool,
    watched_rooms: BTreeSet<OwnedRoomId>,
    ready: BTreeSet<JobBinding>,
}

thread_local! { static SCHEDULER: RefCell<Scheduler> = RefCell::new(Scheduler::default()); }
fn with<R>(f: impl FnOnce(&mut Scheduler) -> R) -> R { SCHEDULER.with(|state| f(&mut state.borrow_mut())) }
fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis().min(u64::MAX as u128) as u64 }

pub fn init() {
    with(|state| {
        match JobStore::open(a2app_core::data_root(), now_ms()) {
            Ok(store) => state.store = Some(store),
            Err(error) => state.error = Some(error),
        }
        state.dirty = true;
        state.revision = state.revision.wrapping_add(1);
    });
}

pub fn revision() -> u64 { with(|state| state.revision) }
pub fn changed() {
    with(|state| { state.dirty = true; state.revision = state.revision.wrapping_add(1); });
    SignalToUI::set_ui_signal();
}

fn current_account() -> Result<String, String> { super::information_flow::account() }
fn job_key(binding: &JobBinding) -> Result<InstanceKey, String> {
    let room = match &binding.context { JobContext::Account => None,
        JobContext::Room { room_id } => Some(room_id), JobContext::Space { space_id } => Some(space_id) };
    Ok((binding.app_id.clone(), room.map(|id| OwnedRoomId::try_from(id.as_str()).map_err(|_| "The task context has an invalid room ID.")).transpose()?))
}
fn flow_context(binding: &JobBinding) -> Result<ContextId, String> {
    let key = job_key(binding)?;
    Ok(ContextId::App { account: binding.account.clone(), app: binding.app_id.clone(), room: key.1.map(|r| r.to_string()) })
}

pub fn app_fingerprint(app_id: &str) -> Result<String, String> {
    with_a2app(|state| state.registry.get(app_id).map(jobs::fingerprint)).flatten()
        .ok_or_else(|| "The mini-app is no longer installed.".to_string())?
}

pub fn snapshot() -> Result<Vec<TaskView>, String> {
    let account = current_account()?;
    let (all, status) = with(|state| {
        if let Some(error) = state.error.as_deref().or_else(|| state.store.as_ref().and_then(JobStore::error)) { return Err(error.to_owned()); }
        let store = state.store.as_ref().ok_or_else(|| "Background tasks are not ready.".to_string())?;
        Ok::<_, String>((store.jobs().iter().filter(|job| job.binding.account == account).cloned().collect::<Vec<_>>(), state.status.clone()))
    })?;
    Ok(all.into_iter().map(|job| {
        let current_fingerprint = app_fingerprint(&job.binding.app_id).ok();
        let status = status.get(&job.id).cloned().unwrap_or_else(|| {
            if job.in_flight.is_some() { "Running; waiting for background.complete.".into() }
            else if !job.enabled { "Paused. Enable this task when you want it to run.".into() }
            else { "Scheduled while Robrix is open and signed in.".into() }
        });
        TaskView { job, current_fingerprint, status }
    }).collect())
}

fn validate_binding(binding: &JobBinding, trigger: &Trigger) -> Result<MiniAppManifest, String> {
    if binding.account != current_account()? { return Err("The account changed. Review the task in the current account.".into()); }
    let key = job_key(binding)?;
    let is_space = matches!(binding.context, JobContext::Space { .. });
    let manifest = with_a2app(|state| {
        if state.permissions.is_restricted(&binding.app_id) { return Err("The mini-app is restricted. Review its App Info before enabling a background task.".into()); }
        state.registry.get(&binding.app_id).cloned().ok_or_else(|| "The mini-app is no longer installed.".to_string())
    }).ok_or("Mini Apps is unavailable.")??;
    if !manifest.can_run_background_in_context(key.1.as_deref().map(|r| r.as_str()), is_space) {
        return Err("This mini-app does not support background work in the selected account, room, or space. Select a supported context or review its source.".into());
    }
    if let Some(room_id) = &key.1 {
        let room = crate::sliding_sync::get_client().and_then(|client| client.get_room(room_id))
            .ok_or("The task's original room or space is unavailable. It must be joined in this account.")?;
        if room.state() != RoomState::Joined || room.is_space() != is_space {
            return Err("The task's original room or space is no longer joined or its context type changed.".into());
        }
    }
    if matches!(trigger, Trigger::RoomMessages) && !matches!(binding.context, JobContext::Room { .. }) {
        return Err("Room-message conditions require one room; spaces and account contexts do not supply a combined message stream.".into());
    }
    Ok(manifest)
}

fn message_permission(binding: &JobBinding, manifest: &MiniAppManifest) -> Result<(), String> {
    let key = job_key(binding)?;
    let room = key.1.as_ref().ok_or("Room-message tasks require a room.")?;
    let cap = a2app_core::capabilities::for_hook("on_room_message").ok_or("The room-message hook is unavailable.")?;
    with_a2app(|state| {
        if state.permissions.room_policy(Some(room.as_str()), RoomAccess::Read) == PolicyDecision::Deny {
            return Err("Room-message input is blocked by Room and space protection. Open Mini Apps > Room and space protection and inspect this room’s read rules.".into());
        }
        if state.permissions.effective_capability_in_context(manifest, cap,
            PermissionContext { origin_room: Some(room.as_str()), target_room: Some(room.as_str()) }) != Effective::Granted {
            return Err("Room-message input is not allowed for this mini-app in this room. Open Mini Apps > App Info and allow its room-message capability for this room; also check Room and space protection.".into());
        }
        Ok(())
    }).ok_or("Mini Apps is unavailable.")?
}

pub fn save(cx: &mut Cx, binding: JobBinding, trigger: Trigger, expected_fingerprint: String) -> Result<(), String> {
    let manifest = validate_binding(&binding, &trigger)?;
    if matches!(trigger, Trigger::Alarm { unix_ms } if unix_ms <= now_ms()) { return Err("Choose a future alarm time before saving or enabling this task.".into()); }
    if jobs::fingerprint(&manifest)? != expected_fingerprint { return Err("The mini-app changed after review. Reload its current version and review the task again.".into()); }
    let id = with(|state| state.store.as_mut().ok_or("Background tasks are unavailable.")?
        .enable(binding, trigger, expected_fingerprint, now_ms()))?;
    with(|state| { state.status.remove(&id); state.failures.remove(&id); });
    changed();
    process(cx, &Event::Signal);
    Ok(())
}

fn owned_job(id: u64) -> Result<Job, String> {
    let account = current_account()?;
    with(|state| state.store.as_ref().and_then(|store| store.job(id)).filter(|job| job.binding.account == account).cloned())
        .ok_or_else(|| "This task is not available in the current account.".into())
}

fn terminate_instance(cx: &mut Cx, key: &InstanceKey) {
    if instances::terminate(cx, key) { super::runtime::app_stopped(cx, &key.0); }
}

fn retire(cx: &mut Cx, id: u64) {
    let activation = with(|state| state.active.remove(&id));
    if let Some(active) = activation {
        if instances::heap_of(&active.key) == Some(active.heap)
            && flow::ensure_context_epoch(&active.context, active.epoch).is_ok()
        { terminate_instance(cx, &active.key); }
    }
}

fn stop_job_instance(cx: &mut Cx, job: &Job) {
    retire(cx, job.id);
    if let (Ok(key), Ok(context)) = (job_key(&job.binding), flow_context(&job.binding))
        && instances::context_of_key(&key).as_ref() == Some(&context)
    { terminate_instance(cx, &key); }
}

pub fn set_enabled(cx: &mut Cx, id: u64, enabled: bool, expected_fingerprint: String) -> Result<(), String> {
    let job = owned_job(id)?;
    if enabled {
        if expected_fingerprint != job.fingerprint || app_fingerprint(&job.binding.app_id)? != expected_fingerprint {
            return Err("This mini-app changed. Review its current source and use Save and enable to adopt that version.".into());
        }
        return save(cx, job.binding, job.trigger, expected_fingerprint);
    }
    stop_job_instance(cx, &job);
    with(|state| state.store.as_mut().ok_or("Background tasks are unavailable.")?.disable(&job.binding.account, id, now_ms()))?;
    with(|state| { state.status.insert(id, "Paused by you. Background work will not restart until you enable it.".into()); });
    changed();
    process(cx, &Event::Signal);
    Ok(())
}

pub fn remove(cx: &mut Cx, id: u64) -> Result<(), String> {
    let job = owned_job(id)?;
    stop_job_instance(cx, &job);
    with(|state| {
        let store = state.store.as_mut().ok_or("Background tasks are unavailable.")?;
        store.disable(&job.binding.account, id, now_ms())?;
        store.remove(&job.binding.account, id)
    })?;
    with(|state| { state.status.remove(&id); state.failures.remove(&id); state.last_owners.remove(&id); });
    changed();
    process(cx, &Event::Signal);
    Ok(())
}

pub fn run_now(cx: &mut Cx, id: u64) -> Result<(), String> {
    if with(|state| state.signed_out || state.suspended) { return Err("Background tasks are suspended. Return to Robrix and sign in before running this task.".into()); }
    let job = owned_job(id)?;
    validate_binding(&job.binding, &job.trigger)?;
    let run = with(|state| state.store.as_mut().ok_or("Background tasks are unavailable.")?
        .claim_now(&job.binding.account, id, now_ms(), |app| app_fingerprint(app).ok()))?;
    let Some(run) = run else { return Err("This task is paused, already running, or changed since review.".into()); };
    dispatch(cx, run, "manual", &[]);
    if stop_on_storage_error(cx) { return Err("Background scheduling stopped because its state could not be saved. Restart Robrix after resolving the storage problem.".into()); }
    changed();
    Ok(())
}

/// UI closure only parks an active run. Future triggers recreate a worker.
pub fn retains_instance(key: &InstanceKey) -> bool {
    let Ok(account) = current_account() else { return false };
    with(|state| !state.signed_out && !state.suspended && state.error.is_none() && state.store.as_ref().is_some_and(|store| store.error().is_none() && store.jobs().iter()
        .any(|job| job.in_flight.is_some() && job.binding.account == account && job_key(&job.binding).as_ref() == Ok(key)
            && state.active.get(&job.id).is_some_and(|active| instances::heap_of(key) == Some(active.heap)
                && flow::ensure_context_epoch(&active.context, active.epoch).is_ok()))))
}

pub fn disable_app(cx: &mut Cx, app_id: &str) {
    let Ok(account) = current_account() else { return };
    let ids = with(|state| state.store.as_ref().map(|store| store.jobs().iter().filter(|job|
        job.binding.account == account && job.binding.app_id == app_id).map(|job| job.id).collect::<Vec<_>>()).unwrap_or_default());
    for id in ids {
        if let Ok(job) = owned_job(id) { stop_job_instance(cx, &job); }
        let result = with(|state| state.store.as_mut().unwrap().disable(&account, id, now_ms()));
        if let Err(error) = result { with(|state| state.error = Some(error)); }
    }
    changed();
}

/// In-flight work dies before persistent run state is released on logout.
pub fn suspend(cx: &mut Cx, signed_out: bool) {
    let (account, ids) = with(|state| {
        state.signed_out = signed_out;
        (state.account.take(), state.active.keys().copied().collect::<Vec<_>>())
    });
    for id in ids { retire(cx, id); }
    if let Some(account) = &account {
        let jobs = with(|state| state.store.as_ref().map(|store| store.jobs().iter()
            .filter(|job| &job.binding.account == account).cloned().collect::<Vec<_>>()).unwrap_or_default());
        for job in jobs { stop_job_instance(cx, &job); }
    }
    with(|state| {
        if let Some(timer) = state.timer.take() { cx.stop_timer(timer); }
        state.timer_at = None;
        state.watched_rooms.clear();
        if let (Some(store), Some(account)) = (&mut state.store, account) {
            if let Err(error) = store.interrupt_active(&account, now_ms()) { state.error = Some(error); }
        }
    });
    changed();
}

pub fn lifecycle(cx: &mut Cx, active: bool) {
    if active { with(|state| state.suspended = false); changed(); }
    else {
        with(|state| state.suspended = true);
        suspend(cx, false);
        super::runtime::stop_background_watches();
    }
}

pub fn room_closed(cx: &mut Cx, room: &str) {
    let Ok(account) = current_account() else { return };
    let affected = with(|state| state.store.as_ref().map(|store| store.jobs().iter().filter(|job|
        job.binding.account == account && job_key(&job.binding).ok().and_then(|key| key.1).is_some_and(|id| id.as_str() == room))
        .cloned().collect::<Vec<_>>()).unwrap_or_default());
    for job in affected {
        retire(cx, job.id);
        if let Some(run) = job.in_flight {
            let _ = with(|state| state.store.as_mut().unwrap().complete(&account, job.id, run.run_id, RunOutcome::Interrupted, now_ms()));
        }
        if let Ok(key) = job_key(&job.binding) { terminate_instance(cx, &key); }
    }
    changed();
}

pub fn watched_rooms() -> BTreeSet<OwnedRoomId> { with(|state| state.watched_rooms.clone()) }
pub fn handles_messages(heap: usize) -> bool {
    let Some(key) = instances::key_of_heap(heap) else { return false };
    let Ok(account) = current_account() else { return false };
    with(|state| state.store.as_ref().is_some_and(|store| store.jobs().iter().any(|job|
        (job.enabled || job.in_flight.is_some()) && job.binding.account == account && matches!(job.trigger, Trigger::RoomMessages)
            && job_key(&job.binding).as_ref() == Ok(&key))))
}

fn prepare(cx: &mut Cx, job: &Job) -> Result<(InstanceKey, usize, ContextId, u64), String> {
    let manifest = validate_binding(&job.binding, &job.trigger)?;
    if jobs::fingerprint(&manifest)? != job.fingerprint { return Err("The mini-app changed. Review the current source and save the task again.".into()); }
    if matches!(job.trigger, Trigger::RoomMessages) { message_permission(&job.binding, &manifest)?; }
    let key = job_key(&job.binding)?;
    let context = flow_context(&job.binding)?;
    if instances::context_of_key(&key).is_some_and(|current| current != context) {
        return Err("A different account or public instance currently owns this mini-app surface. Close it before starting the private background context.".into());
    }
    let layout = super::runtime::saved_layout(&instances::tag_of(&key));
    instances::ensure_background(cx, &key, &manifest, &[], layout).ok_or("The background mini-app could not start.")?;
    match (instances::heap_of(&key), flow::context_epoch(&context)) {
        (Some(heap), Ok(epoch)) => Ok((key, heap, context, epoch)),
        _ => { terminate_instance(cx, &key); Err("The background mini-app has no live protected script.".into()) }
    }
}

fn dispatch(cx: &mut Cx, run: JobRun, reason: &str, messages: &[serde_json::Value]) {
    let job = with(|state| state.store.as_ref().and_then(|store| store.job(run.job_id)).cloned());
    let result = (|| -> Result<(), String> {
        let job = job.ok_or("The background task was removed.")?;
        let (key, heap, context, epoch) = prepare(cx, &job)?;
        with(|state| { state.failures.remove(&run.job_id); });
        instances::set_background_running(cx, &key, true);
        with(|state| {
            state.last_owners.retain(|id, (owner_heap, _, _)| *id == run.job_id || *owner_heap != heap);
            state.last_owners.insert(run.job_id, (heap, epoch, context.clone()));
            state.active.insert(run.job_id, Activation { key: key.clone(), heap, context, epoch, run_id: run.run_id, started: Instant::now() });
        });
        // Every message is host-attributed through the same room hook contract
        // before the otherwise-public scheduling hook receives its payload.
        if !messages.is_empty() {
            message_permission(&job.binding, &validate_binding(&job.binding, &job.trigger)?)?;
            super::information_flow::record_hook(heap, live_id!(on_room_message), &[&serde_json::Value::Array(messages.to_vec()).to_string()])?;
        }
        let payload = serde_json::json!({ "run_id": run.run_id, "job_id": run.job_id, "reason": reason,
            "scheduled_at": run.scheduled_ms, "started_at": run.started_ms, "deadline_at": run.deadline_ms, "messages": messages }).to_string();
        if !instances::call_hook(cx, &key, live_id!(on_background), &[&payload]) {
            retire(cx, run.job_id);
            let _ = with(|state| state.store.as_mut().unwrap().pause(&run.binding.account, run.job_id, PauseReason::HookUnavailable, now_ms()));
            return Err("This mini-app did not provide a working on_background hook. Review its source before enabling it again.".into());
        }
        with(|state| { state.status.insert(run.job_id, "Running; waiting for background.complete.".into()); });
        Ok(())
    })();
    if let Err(error) = result {
        retire(cx, run.job_id);
        let _ = with(|state| state.store.as_mut().unwrap().complete(&run.binding.account, run.job_id, run.run_id, RunOutcome::Blocked, now_ms()));
        with(|state| { state.status.insert(run.job_id, error); });
    }
}

pub fn complete(cx: &mut Cx, heap: usize, run_id: u64, success: bool) -> Result<(), String> {
    let context = super::information_flow::context_for_heap(heap)?;
    let found = with(|state| state.active.iter().find(|(_, active)| active.heap == heap && active.run_id == run_id)
        .map(|(id, active)| (*id, active.key.clone(), active.context.clone(), active.epoch)));
    let (id, key, expected, epoch) = found.ok_or("This completion does not belong to an active background run.")?;
    if expected != context { return Err("The background context changed.".into()); }
    flow::ensure_context_epoch(&expected, epoch)?;
    let expired = with(|state| state.store.as_ref().and_then(|store| store.job(id))
        .and_then(|job| job.in_flight.as_ref()).is_none_or(|run| run.deadline_ms <= now_ms())
        || state.active.get(&id).is_none_or(|active| active.started.elapsed().as_millis() >= jobs::RUN_TIMEOUT_MS as u128));
    if expired {
        retire(cx, id);
        let _ = with(|state| state.store.as_mut().unwrap().pause(context.account(), id, PauseReason::TimedOut, now_ms()));
        changed();
        return Err("This background run expired; its old instance was stopped. Review the task before enabling it again.".into());
    }
    let completed = with(|state| state.store.as_mut().ok_or("Background tasks are unavailable.")?
        .complete(context.account(), id, run_id, if success { RunOutcome::Succeeded } else { RunOutcome::Failed }, now_ms()))?;
    if !completed { return Err("This background run expired or was already completed.".into()); }
    with(|state| {
        state.active.remove(&id);
        let status = if success { "Completed successfully.".into() }
            else { state.failures.get(&id).map(|(_, _, error)| error.clone()).unwrap_or_else(|| "The mini-app reported a failure. Open its interface and Protection activity for details.".into()) };
        state.status.insert(id, status);
    });
    instances::set_background_running(cx, &key, false);
    changed();
    Ok(())
}

/// Called after the completion reply, so its callback cannot retain a hidden
/// worker's activation or host capabilities beyond this run.
pub fn retire_completed(cx: &mut Cx, heap: usize) {
    let Some(key) = instances::key_of_heap(heap) else { return };
    let active = with(|state| state.active.values().any(|active| active.heap == heap));
    if !active && instances::pane_state(heap).is_some_and(|pane| !pane.foreground) { terminate_instance(cx, &key); }
}

/// Keep trusted host-denial instructions in memory, never app-authored output.
pub fn record_failure(heap: usize, error: &str) {
    let Ok(account) = current_account() else { return };
    let current = instances::context_of_heap(heap).and_then(|context| flow::context_epoch(&context).ok().map(|epoch| (context, epoch)));
    let mut error = error.chars().take(4096).collect::<String>();
    if error.contains("Data sharing") || error.contains("sensitive action") || error.contains("untrusted") {
        error.push_str(" Open the task's app and keep it open, choose Run now, review the current blocked action in Data sharing, and retry from that live app. A stopped run's one-time approval cannot be reused.");
    }
    with(|state| {
        let ids = state.last_owners.iter().filter(|(_, (owner_heap, epoch, context))|
            *owner_heap == heap && context.account() == account
                && current.as_ref().is_none_or(|(live, live_epoch)| live == context && live_epoch == epoch))
            .map(|(id, (_, epoch, _))| (*id, *epoch)).collect::<Vec<_>>();
        for (id, epoch) in ids {
            state.failures.insert(id, (heap, epoch, error.clone()));
            state.status.insert(id, error.clone());
        }
        state.revision = state.revision.wrapping_add(1);
    });
}

/// Completed ownership is needed only until this event's broker failures drain.
pub fn finish_failure_drain() {
    with(|state| state.last_owners.retain(|id, _| state.active.contains_key(id)));
}

pub fn room_messages(cx: &mut Cx, room: &OwnedRoomId, messages: &[serde_json::Value]) {
    let Ok(account) = current_account() else { return };
    if with(|state| state.signed_out || state.suspended || state.account.as_ref() != Some(&account)) { return; }
    let batch = messages.iter().filter(|message| message["event_id"].as_str().is_some()).take(jobs::MAX_MESSAGE_BATCH).cloned().collect::<Vec<_>>();
    let ids = batch.iter().map(|message| message["event_id"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
    let runs = with(|state| state.store.as_mut().map(|store| store.claim_room_message_batch(&account,
        room.as_str(), &ids, now_ms(), DISPATCH_BATCH, |app| app_fingerprint(app).ok())));
    if let Some(Ok(runs)) = runs {
        for (run, indexes) in runs {
            let unseen = indexes.into_iter().filter_map(|index| batch.get(index).cloned()).collect::<Vec<_>>();
            dispatch(cx, run, "room_messages", &unseen);
            if stop_on_storage_error(cx) { return; }
        }
    }
    if stop_on_storage_error(cx) { return; }
    changed();
}

fn stop_on_storage_error(cx: &mut Cx) -> bool {
    let (failed, first, jobs) = with(|state| {
        let failed = state.error.is_some() || state.store.as_ref().and_then(JobStore::error).is_some();
        let first = failed && !state.stopped_on_error;
        if first { state.stopped_on_error = true; }
        let jobs = if first { state.store.as_ref().map(|store| store.jobs().to_vec()).unwrap_or_default() } else { Vec::new() };
        (failed, first, jobs)
    });
    if first {
        // Never release a durable claim after its commit failed. Stop execution;
        // reopening the store will recover its uncertain claim as interrupted.
        for job in jobs { stop_job_instance(cx, &job); }
        with(|state| {
            if let Some(timer) = state.timer.take() { cx.stop_timer(timer); }
            state.timer_at = None;
            state.watched_rooms.clear();
            state.revision = state.revision.wrapping_add(1);
        });
        super::runtime::refresh_background_watches();
    }
    failed
}

/// Called on event boundaries; non-timer events only reconcile invalidations.
pub fn process(cx: &mut Cx, event: &Event) {
    if stop_on_storage_error(cx) { return; }
    let current = current_account().ok();
    let switch = with(|state| state.account != current && !state.signed_out && !state.suspended);
    if switch {
        suspend(cx, false);
        with(|state| state.account = current.clone());
    }
    let (due, reconcile) = with(|state| {
        let fired = state.timer.is_some_and(|timer| timer.is_event(event).is_some());
        if fired { state.timer = None; state.timer_at = None; }
        let reconcile = state.dirty || state.retry_at.is_some_and(|at| at <= now_ms());
        let due = state.dirty || fired;
        state.dirty = false;
        (due && !state.signed_out && !state.suspended && state.account.is_some() && state.store.is_some()
            && state.error.is_none() && state.store.as_ref().and_then(JobStore::error).is_none(), reconcile)
    });
    if !due || !instances::host_template_ready() { return; }
    let Some(account) = current else { return };
    let now = now_ms();
    let expired = with(|state| {
        let mut expired = state.store.as_ref().unwrap().expired_runs(&account, now).into_iter().map(|run| run.job_id).collect::<BTreeSet<_>>();
        expired.extend(state.active.iter().filter(|(_, active)| active.started.elapsed().as_millis() >= jobs::RUN_TIMEOUT_MS as u128).map(|(id, _)| *id));
        expired
    });
    for id in expired {
        retire(cx, id);
        let result = with(|state| state.store.as_mut().unwrap().pause(&account, id, PauseReason::TimedOut, now));
        if let Err(error) = result { with(|state| state.error = Some(error)); }
        with(|state| { state.status.insert(id, "Paused: the task did not complete within two minutes. Its old instance was stopped before releasing this run.".into()); });
    }
    if stop_on_storage_error(cx) { return; }
    let active_lost = with(|state| state.active.iter().filter(|(_, active)| instances::heap_of(&active.key) != Some(active.heap)
        || flow::ensure_context_epoch(&active.context, active.epoch).is_err()).map(|(id, active)| (*id, active.run_id)).collect::<Vec<_>>());
    for (id, run) in active_lost {
        retire(cx, id);
        let _ = with(|state| state.store.as_mut().unwrap().complete(&account, id, run, RunOutcome::Interrupted, now));
    }
    if reconcile {
    let candidates = with(|state| state.store.as_ref().unwrap().jobs().iter().filter(|job| job.binding.account == account && (job.enabled || job.in_flight.is_some())).cloned().collect::<Vec<_>>());
    let mut watched = BTreeSet::new();
    let mut ready_bindings = BTreeSet::new();
    let mut retry = false;
    for job in candidates {
        let current = app_fingerprint(&job.binding.app_id).ok();
        if current.as_ref() != Some(&job.fingerprint) {
            stop_job_instance(cx, &job);
            let reason = if current.is_some() { PauseReason::AppChanged } else { PauseReason::AppMissing };
            let _ = with(|state| state.store.as_mut().unwrap().pause(&account, job.id, reason, now));
            with(|state| { state.status.insert(job.id, "Paused: the mini-app is missing or its source changed. Review its current version before saving and enabling this task.".into()); });
            continue;
        }
        let ready = validate_binding(&job.binding, &job.trigger).and_then(|manifest| {
            if matches!(job.trigger, Trigger::RoomMessages) { message_permission(&job.binding, &manifest)?; }
            Ok(())
        });
        match ready {
            Ok(()) => {
                ready_bindings.insert(job.binding.clone());
                if matches!(job.trigger, Trigger::RoomMessages)
                    && let Ok((_, Some(room))) = job_key(&job.binding) { watched.insert(room); }
            }
            Err(error) => { retry = true; with(|state| { state.status.insert(job.id, error); }); }
        }
    }
    let watch_changed = with(|state| {
        let changed = state.watched_rooms != watched;
        state.watched_rooms = watched;
        state.ready = ready_bindings;
        changed
    });
    if watch_changed { super::runtime::refresh_background_watches(); }
    with(|state| state.retry_at = retry.then(|| now.saturating_add(RECONCILE_RETRY_MS)));
    }
    let runs = with(|state| {
        let ready = &state.ready;
        state.store.as_mut().unwrap().claim_due_ready(&account, now, DISPATCH_BATCH,
            |app| app_fingerprint(app).ok(), |binding| ready.contains(binding))
    });
    match runs {
        Ok(runs) => for run in runs {
            let reason = if matches!(run.trigger, Trigger::Alarm { .. }) { "alarm" } else { "interval" };
            dispatch(cx, run, reason, &[]);
            if stop_on_storage_error(cx) { return; }
        },
        Err(error) => with(|state| state.error = Some(error)),
    }
    if stop_on_storage_error(cx) { return; }
    with(|state| {
        let scheduled = state.store.as_ref().and_then(|store| store.next_wakeup_ready(&account, |binding| state.ready.contains(binding)));
        let monotonic = state.active.values().map(|active| now.saturating_add(jobs::RUN_TIMEOUT_MS.saturating_sub(active.started.elapsed().as_millis().min(u64::MAX as u128) as u64))).min();
        let next = if state.dirty { Some(now.saturating_add(50)) } else {
            scheduled.into_iter().chain(state.retry_at).chain(monotonic).min().map(|at| at.min(now.saturating_add(CLOCK_RECHECK_MS)))
        };
        if next != state.timer_at {
            if let Some(timer) = state.timer.take() { cx.stop_timer(timer); }
            state.timer_at = next;
            if let Some(next) = next { state.timer = Some(cx.start_timeout((next.saturating_sub(now).max(50) as f64) / 1000.0)); }
        }
        state.revision = state.revision.wrapping_add(1);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture { cx: Cx, root: std::path::PathBuf, binding: JobBinding, fingerprint: String }
    impl Fixture {
        fn new(source: &str) -> Self {
            static NEXT_FIXTURE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let nonce = format!("{}-{}", std::process::id(), NEXT_FIXTURE.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
            let root = std::env::temp_dir().join(format!("robrix-background-runtime-{}-{nonce}", std::process::id()));
            let mut manifest = a2app_core::builtin::stock("reminder").unwrap();
            manifest.id = format!("background-test-{nonce}");
            manifest.source = source.into();
            manifest.permissions.clear();
            manifest.capabilities.clear();
            let fingerprint = jobs::fingerprint(&manifest).unwrap();
            let binding = JobBinding { account: "@background-test:example.org".into(), app_id: manifest.id.clone(), context: JobContext::Account };
            super::super::information_flow::TEST_ACCOUNT.with(|account| *account.borrow_mut() = Some(binding.account.clone()));
            super::super::runtime::initialize_background_test(manifest);
            with(|state| *state = Scheduler { store: Some(JobStore::open(&root, now_ms()).unwrap()), account: Some(binding.account.clone()), ..Default::default() });
            let mut cx = Cx::new(Box::new(|_, _| {}));
            let template = cx.with_vm(|vm| {
                makepad_widgets::script_mod(vm);
                crate::shared::script_mod(vm);
                super::super::host_set::script_mod(vm);
                let value = script_eval!(vm, { mod.widgets.MiniAppHost {} });
                vm.bx.heap.new_object_ref(value.as_object().unwrap())
            });
            instances::set_host_template(template);
            Self { cx, root, binding, fingerprint }
        }
        fn enable(&mut self) -> u64 {
            save(&mut self.cx, self.binding.clone(), Trigger::Interval { seconds: 60 }, self.fingerprint.clone()).unwrap();
            snapshot().unwrap()[0].job.id
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            suspend(&mut self.cx, true);
            instances::quit_everything(&mut self.cx);
            instances::clear_host_template();
            with(|state| *state = Scheduler::default());
            super::super::information_flow::TEST_ACCOUNT.with(|account| *account.borrow_mut() = None);
            let _ = std::fs::remove_dir_all(&self.root);
            makepad_widgets::splash_host::take_splash_host_requests();
        }
    }

    #[test]
    fn real_parked_splash_completes_through_broker_and_reuses_foreground_instance() {
        let mut fixture = Fixture::new(r#"// background: true
let initial = host.request("env", {})
fn on_background(json){
    let run = json.parse_json().run_id
    host.request("background.complete", {run_id: run, success: true})
}
View{width: Fill height: Fill}
"#);
        let id = fixture.enable();
        let key = job_key(&fixture.binding).unwrap();
        with_a2app(|state| state.permissions.grant_once(&key.0, a2app_core::permissions::Permission::Notifications));
        assert!(instances::heap_of(&key).is_none(), "saving a future schedule must not evaluate source");
        run_now(&mut fixture.cx, id).unwrap();
        let heap = instances::heap_of(&key).unwrap();
        assert!(!instances::is_foreground(&key.0));
        let run = snapshot().unwrap()[0].job.in_flight.clone().unwrap();
        assert!(run_now(&mut fixture.cx, id).is_err(), "a pending completion retains the exclusive run");
        super::super::runtime::process_background_test_broker(&mut fixture.cx);
        let job = snapshot().unwrap().remove(0).job;
        assert!(job.in_flight.is_none());
        assert_eq!(job.last_run.unwrap().outcome, RunOutcome::Succeeded);
        assert!(complete(&mut fixture.cx, heap, run.run_id, true).is_err(), "completion cannot replay");
        assert!(instances::heap_of(&key).is_none(), "a hidden completed worker cannot retain timers outside its run");
        record_failure(heap, a2app_core::information_flow::ACTION_REVIEW_REQUIRED);
        assert!(snapshot().unwrap()[0].status.contains("keep it open"), "the host denial remains attributed after hidden-worker retirement");
        finish_failure_drain();
        record_failure(heap, "An unrelated later error");
        assert!(!snapshot().unwrap()[0].status.contains("unrelated"));
        assert!(!with_a2app(|state| state.permissions.has_once(&key.0, a2app_core::permissions::Permission::Notifications)).unwrap(), "legacy one-time grants expire with the final hidden isolate");
        let manifest = with_a2app(|state| state.registry.get(&fixture.binding.app_id).cloned()).flatten().unwrap();
        instances::ensure(&mut fixture.cx, &key, &manifest, &[], Default::default()).unwrap();
        makepad_widgets::splash_host::take_splash_host_requests();
        let heap = instances::heap_of(&key).unwrap();
        let owner = WidgetUid(444);
        instances::adopt(&mut fixture.cx, &key, owner, instances::Surface::Modal).unwrap();
        assert!(instances::is_foreground(&key.0));
        run_now(&mut fixture.cx, id).unwrap();
        assert_eq!(instances::heap_of(&key), Some(heap), "showing a scheduled app reuses its isolate");
        let requests = makepad_widgets::splash_host::take_splash_host_requests();
        assert!(requests.iter().all(|request| !request.may_prompt), "scheduled work suppresses prompts even when visible");
        set_enabled(&mut fixture.cx, id, false, fixture.fingerprint.clone()).unwrap();
        assert!(instances::heap_of(&key).is_none(), "Pause retires the actual foreground isolate before clearing the claim");
        assert!(snapshot().unwrap()[0].job.in_flight.is_none());
        assert!(!snapshot().unwrap()[0].job.enabled);
    }

    #[test]
    fn lifecycle_restore_pause_and_remove_retire_idle_and_pending_activations() {
        let mut fixture = Fixture::new("// background: true\nfn on_background(json){}\nView{width: Fill height: Fill}");
        let id = fixture.enable();
        let key = job_key(&fixture.binding).unwrap();
        let context = flow_context(&fixture.binding).unwrap();
        run_now(&mut fixture.cx, id).unwrap();
        let epoch = flow::context_epoch(&context).unwrap();
        let old_heap = instances::heap_of(&key).unwrap();
        let old_run = snapshot().unwrap()[0].job.in_flight.clone().unwrap().run_id;
        let manifest = with_a2app(|state| state.registry.get(&fixture.binding.app_id).cloned()).flatten().unwrap();
        assert!(instances::ensure_public(&mut fixture.cx, &manifest, &[], Default::default()).is_none(), "a public launch must never reuse a private background heap");
        lifecycle(&mut fixture.cx, false);
        assert!(instances::heap_of(&key).is_none(), "OS suspension also retires idle scheduled scripts");
        process(&mut fixture.cx, &Event::Signal);
        assert!(instances::heap_of(&key).is_none(), "signals do not restart a suspended scheduler");
        assert!(snapshot().unwrap()[0].job.enabled, "suspension preserves desired enablement");
        drop(with(|state| state.store.take()));
        with(|state| state.store = Some(JobStore::open(&fixture.root, now_ms()).unwrap()));
        assert_eq!(snapshot().unwrap()[0].job.last_run.as_ref().unwrap().outcome, RunOutcome::Interrupted);
        lifecycle(&mut fixture.cx, true);
        process(&mut fixture.cx, &Event::Signal);
        assert!(instances::heap_of(&key).is_none(), "restore waits for a durable trigger claim before evaluating source");
        run_now(&mut fixture.cx, id).unwrap();
        assert_ne!(flow::context_epoch(&context).unwrap(), epoch);
        assert!(complete(&mut fixture.cx, old_heap, old_run, true).is_err(), "a completion before restart cannot finish the restored run");
        let heap = instances::heap_of(&key).unwrap();
        let run = snapshot().unwrap()[0].job.in_flight.clone().unwrap();
        remove(&mut fixture.cx, id).unwrap();
        assert!(snapshot().unwrap().is_empty(), "Remove cancels the persisted run before deletion");
        assert!(instances::heap_of(&key).is_none());
        assert!(complete(&mut fixture.cx, heap, run.run_id, true).is_err());
        let id = fixture.enable();
        let manifest = with_a2app(|state| state.registry.get(&fixture.binding.app_id).cloned()).flatten().unwrap();
        instances::ensure_background(&mut fixture.cx, &key, &manifest, &[], Default::default()).unwrap();
        set_enabled(&mut fixture.cx, id, false, fixture.fingerprint.clone()).unwrap();
        assert!(instances::heap_of(&key).is_none(), "Pause also terminates a currently idle app");
    }
    #[test]
    fn host_deadlines_and_storage_errors_stop_workers_without_releasing_uncertain_claims() {
        let mut fixture = Fixture::new("// background: true\nfn on_background(json){}\nView{width: Fill height: Fill}");
        let id = fixture.enable();
        run_now(&mut fixture.cx, id).unwrap();
        let key = job_key(&fixture.binding).unwrap();
        with(|state| state.active.get_mut(&id).unwrap().started = Instant::now() - std::time::Duration::from_millis(jobs::RUN_TIMEOUT_MS));
        changed();
        process(&mut fixture.cx, &Event::Signal);
        assert!(instances::heap_of(&key).is_none());
        assert_eq!(snapshot().unwrap()[0].job.pause_reason, Some(PauseReason::TimedOut));
        set_enabled(&mut fixture.cx, id, true, fixture.fingerprint.clone()).unwrap();
        run_now(&mut fixture.cx, id).unwrap();
        assert!(instances::heap_of(&key).is_some());
        with(|state| state.error = Some("Injected storage failure".into()));
        process(&mut fixture.cx, &Event::Signal);
        assert!(instances::heap_of(&key).is_none());
        assert!(!retains_instance(&key));
        assert!(snapshot().is_err());
        assert!(with(|state| state.store.as_ref().unwrap().job(id).unwrap().in_flight.is_some()), "uncertain durable claim remains recoverable after a storage error");
    }

    #[test]
    fn missing_hook_and_source_changes_pause_without_autostarting_new_source() {
        let mut fixture = Fixture::new("// background: true\nView{width: Fill height: Fill}");
        let id = fixture.enable();
        run_now(&mut fixture.cx, id).unwrap();
        assert_eq!(snapshot().unwrap()[0].job.pause_reason, Some(PauseReason::HookUnavailable));
        assert!(instances::heap_of(&job_key(&fixture.binding).unwrap()).is_none());
        set_enabled(&mut fixture.cx, id, true, fixture.fingerprint.clone()).unwrap();
        with_a2app(|state| {
            let mut manifest = state.registry.get(&fixture.binding.app_id).cloned().unwrap();
            manifest.source.push_str("\n// Changed after review");
            state.registry.insert(manifest);
        });
        changed();
        process(&mut fixture.cx, &Event::Signal);
        assert_eq!(snapshot().unwrap()[0].job.pause_reason, Some(PauseReason::AppChanged));
        assert!(run_now(&mut fixture.cx, id).is_err());
        assert!(instances::heap_of(&job_key(&fixture.binding).unwrap()).is_none());
    }

}
