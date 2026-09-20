//! Host-owned, opt-in schedules for bounded mini-app background work.
//!
//! Claims are persisted before dispatch. Missed intervals coalesce into one
//! run; a restored claim is interrupted, never replayed as that run. An alarm
//! whose completion is uncertain stays disabled. These are best-effort jobs,
//! not an exactly-once execution service. The runtime must stop an isolate
//! before disabling, interrupting or removing its running job.

use std::{collections::{BTreeMap, BTreeSet}, fs::{self, File, OpenOptions}, io::{Read, Write}, path::{Path, PathBuf}, sync::atomic::{AtomicU64, Ordering}};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use crate::manifest::MiniAppManifest;

pub const MAX_JOBS: usize = 64;
pub const MIN_INTERVAL_SECONDS: u64 = 60;
pub const MAX_CONCURRENT_RUNS: usize = 4;
pub const RUN_TIMEOUT_MS: u64 = 120_000;
pub const MAX_MESSAGE_BATCH: usize = 32;
/// Splash receives JSON numbers as doubles; identities must round-trip exactly.
pub const MAX_ID: u64 = (1 << 53) - 1;
const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_RECENT_EVENTS: usize = 32;
const SCHEMA: u32 = 1;
const FILE_NAME: &str = "background_jobs.json";

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum JobContext {
    Account,
    Room { room_id: String },
    Space { space_id: String },
}

impl JobContext {
    pub fn room_id(&self) -> Option<&str> {
        match self { Self::Account => None, Self::Room { room_id } => Some(room_id), Self::Space { space_id } => Some(space_id) }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobBinding {
    pub account: String,
    pub app_id: String,
    pub context: JobContext,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Trigger {
    Interval { seconds: u64 },
    Alarm { unix_ms: u64 },
    /// New events in the exact bound room; space bindings are not supported.
    /// No timeline backfill or descendant-space subscription is implied.
    RoomMessages,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RunOutcome { Succeeded, Failed, Blocked, Cancelled, Interrupted }

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PauseReason { AppChanged, AppMissing, TimedOut, HookUnavailable }

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunMetadata {
    pub run_id: u64,
    pub started_ms: u64,
    pub scheduled_ms: u64,
    pub deadline_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    pub run_id: u64,
    pub started_ms: u64,
    pub finished_ms: u64,
    pub outcome: RunOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub id: u64,
    pub binding: JobBinding,
    pub trigger: Trigger,
    pub enabled: bool,
    pub fingerprint: String,
    pub next_due_ms: Option<u64>,
    pub in_flight: Option<RunMetadata>,
    pub last_run: Option<RunRecord>,
    pub pause_reason: Option<PauseReason>,
    recent_events: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobRun {
    pub job_id: u64,
    pub run_id: u64,
    pub binding: JobBinding,
    pub trigger: Trigger,
    pub started_ms: u64,
    pub scheduled_ms: u64,
    pub deadline_ms: u64,
    pub fingerprint: String,
}

impl Job {
    fn current_run(&self) -> Option<JobRun> {
        self.in_flight.as_ref().map(|run| JobRun {
            job_id: self.id, run_id: run.run_id, binding: self.binding.clone(), trigger: self.trigger.clone(),
            started_ms: run.started_ms, scheduled_ms: run.scheduled_ms, deadline_ms: run.deadline_ms,
            fingerprint: self.fingerprint.clone(),
        })
    }

    fn finish(&mut self, outcome: RunOutcome, now_ms: u64) {
        if let Some(run) = self.in_flight.take() {
            self.last_run = Some(RunRecord { run_id: run.run_id, started_ms: run.started_ms,
                finished_ms: now_ms.max(run.started_ms), outcome });
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    schema: u32,
    next_job_id: u64,
    next_run_id: u64,
    jobs: Vec<Job>,
}

impl Default for Document {
    fn default() -> Self { Self { schema: SCHEMA, next_job_id: 1, next_run_id: 1, jobs: Vec::new() } }
}

/// One process owns this store. Persistence failure disables further mutation
/// and dispatch until a successful reopen, including ambiguous rename errors.
pub struct JobStore {
    path: PathBuf,
    document: Document,
    error: Option<String>,
    _lock: File,
}

impl JobStore {
    pub fn open(root: impl AsRef<Path>, now_ms: u64) -> Result<Self, String> {
        let path = root.as_ref().join(FILE_NAME);
        fs::create_dir_all(root.as_ref()).map_err(|_| "Cannot create background job storage.")?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
        let lock = options.open(root.as_ref().join(".background_jobs.lock")).map_err(|_| "Cannot open the background scheduler lock.")?;
        restrict_file(&lock).map_err(|_| "Cannot make the background scheduler lock private.")?;
        lock.try_lock().map_err(|_| "Background scheduling is already active in another process, or its exclusive lock is unavailable.")?;
        let document = match File::open(&path) {
            Ok(file) => {
                let metadata = file.metadata().map_err(|_| "Cannot inspect background job settings.")?;
                if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES as u64 { return Err("Background job settings exceed their storage limit.".into()); }
                restrict_file(&file).map_err(|_| "Cannot make background job settings private.")?;
                let mut bytes = Vec::new();
                file.take(MAX_FILE_BYTES as u64 + 1).read_to_end(&mut bytes).map_err(|_| "Cannot read background job settings.")?;
                if bytes.len() > MAX_FILE_BYTES { return Err("Background job settings exceed their storage limit.".into()); }
                serde_json::from_slice::<Document>(&bytes).map_err(|_| "Background job settings are invalid. They have not been overwritten or enabled.")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Document::default(),
            Err(_) => return Err("Cannot open background job settings. No jobs were enabled.".into()),
        };
        validate_document(&document)?;
        let mut store = Self { path, document, error: None, _lock: lock };
        let mut recovered = store.document.clone();
        let mut changed = false;
        for job in &mut recovered.jobs {
            if job.in_flight.is_none() { continue; }
            job.finish(RunOutcome::Interrupted, now_ms);
            recover_schedule(job, now_ms)?;
            changed = true;
        }
        if changed { store.persist(recovered)?; }
        Ok(store)
    }

    pub fn jobs(&self) -> &[Job] { &self.document.jobs }
    pub fn job(&self, id: u64) -> Option<&Job> { self.document.jobs.iter().find(|job| job.id == id) }
    pub fn error(&self) -> Option<&str> { self.error.as_deref() }

    /// Explicit user approval adopts this exact installed source and contract.
    ///
    /// A binding keeps its stable ID when its trigger or reviewed app changes.
    pub fn enable(&mut self, binding: JobBinding, trigger: Trigger, fingerprint: String, now_ms: u64) -> Result<u64, String> {
        self.healthy()?;
        validate_binding(&binding)?;
        validate_trigger(&trigger, &binding.context)?;
        validate_fingerprint(&fingerprint)?;
        let due = initial_due(&trigger, now_ms)?;
        let mut next = self.document.clone();
        if let Some(job) = next.jobs.iter_mut().find(|job| same_binding(&job.binding, &binding)) {
            if job.in_flight.is_some() { return Err("Stop the running background job before changing its settings.".into()); }
            job.trigger = trigger;
            job.binding = binding;
            job.fingerprint = fingerprint;
            job.enabled = true;
            job.pause_reason = None;
            job.next_due_ms = due;
            let id = job.id;
            self.persist(next)?;
            return Ok(id);
        }
        if next.jobs.len() >= MAX_JOBS { return Err(format!("At most {MAX_JOBS} background jobs can be saved.")); }
        let id = next.next_job_id;
        next.next_job_id = id.checked_add(1).filter(|next| *next <= MAX_ID).ok_or("Background job identities are exhausted.")?;
        next.jobs.push(Job { id, binding, trigger, enabled: true, fingerprint, next_due_ms: due,
            in_flight: None, last_run: None, pause_reason: None, recent_events: Vec::new() });
        self.persist(next)?;
        Ok(id)
    }

    pub fn disable(&mut self, account: &str, id: u64, now_ms: u64) -> Result<(), String> {
        self.set_paused(account, id, None, RunOutcome::Cancelled, now_ms)
    }

    pub fn pause(&mut self, account: &str, id: u64, reason: PauseReason, now_ms: u64) -> Result<(), String> {
        self.set_paused(account, id, Some(reason), RunOutcome::Interrupted, now_ms)
    }

    fn set_paused(&mut self, account: &str, id: u64, reason: Option<PauseReason>, outcome: RunOutcome, now_ms: u64) -> Result<(), String> {
        self.healthy()?;
        let mut next = self.document.clone();
        let job = find_job(&mut next, account, id)?;
        job.finish(outcome, now_ms);
        job.enabled = false;
        job.next_due_ms = None;
        job.pause_reason = reason;
        self.persist(next)
    }

    pub fn remove(&mut self, account: &str, id: u64) -> Result<(), String> {
        self.healthy()?;
        let mut next = self.document.clone();
        if find_job(&mut next, account, id)?.in_flight.is_some() { return Err("Stop the running background job before removing it.".into()); }
        next.jobs.retain(|job| job.id != id);
        self.persist(next)
    }

    /// Advance the schedule and save the run identity before returning work.
    pub fn claim_due(&mut self, account: &str, now_ms: u64, limit: usize,
        current_fingerprint: impl Fn(&str) -> Option<String>) -> Result<Vec<JobRun>, String>
    {
        self.claim_due_ready(account, now_ms, limit, current_fingerprint, |_| true)
    }

    /// Temporarily unavailable bindings retain their schedule without consuming
    /// a one-time alarm; the runtime retries readiness separately.
    pub fn claim_due_ready(&mut self, account: &str, now_ms: u64, limit: usize,
        current_fingerprint: impl Fn(&str) -> Option<String>, is_ready: impl Fn(&JobBinding) -> bool) -> Result<Vec<JobRun>, String>
    {
        self.claim_matching(account, now_ms, limit, current_fingerprint,
            |job| is_ready(&job.binding) && job.next_due_ms.is_some_and(|due| due <= now_ms), None)
    }

    pub fn claim_room_messages(&mut self, account: &str, room_id: &str, event_id: &str, now_ms: u64, limit: usize,
        current_fingerprint: impl Fn(&str) -> Option<String>) -> Result<Vec<JobRun>, String>
    {
        self.claim_room_message_batch(account, room_id, &[event_id.to_owned()], now_ms, limit, current_fingerprint)
            .map(|runs| runs.into_iter().map(|(run, _)| run).collect())
    }

    /// Claim one run for each job's unseen events, returning their input indexes.
    ///
    /// Busy jobs do not queue messages or record them as delivered. The caller
    /// delivers only these indexes and supplies live events, never backfill.
    pub fn claim_room_message_batch(&mut self, account: &str, room_id: &str, event_ids: &[String], now_ms: u64, limit: usize,
        current_fingerprint: impl Fn(&str) -> Option<String>) -> Result<Vec<(JobRun, Vec<usize>)>, String>
    {
        if event_ids.len() > MAX_MESSAGE_BATCH { return Err("Background room event batch exceeds its limit.".into()); }
        for event in event_ids { validate_text(event, 256).map_err(|_| "The room event identity is invalid.")?; }
        let mut indexes = self.document.jobs.iter().filter(|job| job.binding.account == account
            && job.trigger == Trigger::RoomMessages && job.binding.context.room_id() == Some(room_id))
            .filter_map(|job| {
                let unseen = unseen_event_indexes(job, event_ids);
                (!unseen.is_empty()).then_some((job.id, unseen))
            }).collect::<BTreeMap<_, _>>();
        let runs = self.claim_matching(account, now_ms, limit, current_fingerprint,
            |job| indexes.contains_key(&job.id), Some(event_ids))?;
        Ok(runs.into_iter().map(|run| {
            let unseen = indexes.remove(&run.job_id).expect("claimed job has unseen events");
            (run, unseen)
        }).collect())
    }

    pub fn claim_now(&mut self, account: &str, id: u64, now_ms: u64,
        current_fingerprint: impl Fn(&str) -> Option<String>) -> Result<Option<JobRun>, String>
    {
        self.healthy()?;
        let job = self.document.jobs.iter().find(|job| job.id == id && job.binding.account == account).ok_or("Background job not found for this account.")?;
        if !job.enabled { return Err("Enable and review this background job before running it.".into()); }
        Ok(self.claim_matching(account, now_ms, 1, current_fingerprint, |job| job.id == id, None)?.pop())
    }

    fn claim_matching(&mut self, account: &str, now_ms: u64, limit: usize,
        current_fingerprint: impl Fn(&str) -> Option<String>, eligible: impl Fn(&Job) -> bool,
        event_ids: Option<&[String]>) -> Result<Vec<JobRun>, String>
    {
        self.healthy()?;
        let active = self.document.jobs.iter().filter(|job| job.in_flight.is_some()).count();
        let limit = limit.min(MAX_CONCURRENT_RUNS.saturating_sub(active));
        if limit == 0 { return Ok(Vec::new()); }
        let mut next = self.document.clone();
        let mut runs = Vec::new();
        let mut changed = false;
        let mut candidates = next.jobs.iter().enumerate().filter(|(_, job)|
            job.binding.account == account && job.enabled && job.in_flight.is_none() && eligible(job))
            .map(|(index, job)| (index, job.next_due_ms.unwrap_or(now_ms), job.last_run.as_ref().map_or(0, |run| run.finished_ms), job.id))
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(_, due, finished, id)| (*due, *finished, *id));
        for (index, _, _, _) in candidates {
            if runs.len() == limit { break; }
            let job = &mut next.jobs[index];
            let installed = current_fingerprint(&job.binding.app_id);
            if installed.as_deref() != Some(job.fingerprint.as_str()) {
                job.enabled = false;
                job.next_due_ms = None;
                job.pause_reason = Some(if installed.is_some() { PauseReason::AppChanged } else { PauseReason::AppMissing });
                changed = true;
                continue;
            }
            let run_id = next.next_run_id;
            next.next_run_id = run_id.checked_add(1).filter(|next| *next <= MAX_ID).ok_or("Background run identities are exhausted.")?;
            let deadline_ms = now_ms.checked_add(RUN_TIMEOUT_MS).ok_or("Background run deadline exceeds the supported clock range.")?;
            let scheduled_ms = job.next_due_ms.filter(|due| *due <= now_ms).unwrap_or(now_ms);
            job.next_due_ms = match job.trigger {
                Trigger::Interval { seconds } => Some(interval_due(seconds, now_ms)?),
                Trigger::Alarm { .. } => { job.enabled = false; None }
                Trigger::RoomMessages => None,
            };
            job.in_flight = Some(RunMetadata { run_id, started_ms: now_ms, scheduled_ms, deadline_ms });
            if let Some(event_ids) = event_ids {
                for index in unseen_event_indexes(job, event_ids) {
                    if job.recent_events.len() == MAX_RECENT_EVENTS { job.recent_events.remove(0); }
                    job.recent_events.push(event_ids[index].clone());
                }
            }
            runs.push(job.current_run().expect("run assigned above"));
            changed = true;
        }
        if changed { self.persist(next)?; }
        Ok(runs)
    }

    /// Late completions cannot finish a replacement run or another account.
    pub fn complete(&mut self, account: &str, id: u64, run_id: u64, outcome: RunOutcome, now_ms: u64) -> Result<bool, String> {
        self.healthy()?;
        let Some(job) = self.document.jobs.iter().find(|job| job.id == id && job.binding.account == account) else { return Ok(false) };
        if job.in_flight.as_ref().map(|run| run.run_id) != Some(run_id) { return Ok(false); }
        let mut next = self.document.clone();
        find_job(&mut next, account, id)?.finish(outcome, now_ms);
        self.persist(next)?;
        Ok(true)
    }

    /// Call after stopping the account's workers, preserving desired state.
    pub fn interrupt_active(&mut self, account: &str, now_ms: u64) -> Result<usize, String> {
        self.healthy()?;
        let mut next = self.document.clone();
        let mut interrupted = 0;
        for job in &mut next.jobs {
            if job.binding.account != account || job.in_flight.is_none() { continue; }
            job.finish(RunOutcome::Interrupted, now_ms);
            recover_schedule(job, now_ms)?;
            interrupted += 1;
        }
        if interrupted != 0 { self.persist(next)?; }
        Ok(interrupted)
    }

    /// The runtime must retire these workers before clearing their claims.
    pub fn expired_runs(&self, account: &str, now_ms: u64) -> Vec<JobRun> {
        self.document.jobs.iter().filter(|job| job.binding.account == account
            && job.in_flight.as_ref().is_some_and(|run| run.deadline_ms <= now_ms))
            .filter_map(Job::current_run).collect()
    }

    pub fn next_wakeup(&self, account: &str) -> Option<u64> {
        self.next_wakeup_ready(account, |_| true)
    }

    /// Active deadlines always wake the runtime. Unavailable schedules wait for
    /// its readiness retry rather than spinning on a past-due timestamp.
    pub fn next_wakeup_ready(&self, account: &str, is_ready: impl Fn(&JobBinding) -> bool) -> Option<u64> {
        if self.error.is_some() { return None; }
        let capacity = self.document.jobs.iter().filter(|job| job.in_flight.is_some()).count() < MAX_CONCURRENT_RUNS;
        self.document.jobs.iter().filter(|job| job.binding.account == account).filter_map(|job| {
            job.in_flight.as_ref().map(|run| run.deadline_ms)
                .or_else(|| (capacity && job.enabled && is_ready(&job.binding)).then_some(job.next_due_ms).flatten())
        }).min()
    }

    fn healthy(&self) -> Result<(), String> {
        self.error.as_ref().map_or(Ok(()), |error| Err(error.clone()))
    }

    fn persist(&mut self, document: Document) -> Result<(), String> {
        validate_document(&document)?;
        if let Err(error) = write_document(&self.path, &document) {
            self.error = Some(error.clone());
            return Err(error);
        }
        self.document = document;
        Ok(())
    }
}

fn find_job<'a>(document: &'a mut Document, account: &str, id: u64) -> Result<&'a mut Job, String> {
    document.jobs.iter_mut().find(|job| job.id == id && job.binding.account == account)
        .ok_or_else(|| "Background job not found for this account.".into())
}

fn same_binding(left: &JobBinding, right: &JobBinding) -> bool {
    left.account == right.account && left.app_id == right.app_id && left.context.room_id() == right.context.room_id()
}

fn unseen_event_indexes(job: &Job, event_ids: &[String]) -> Vec<usize> {
    let mut seen = job.recent_events.iter().map(String::as_str).collect::<BTreeSet<_>>();
    event_ids.iter().enumerate().filter_map(|(index, event)| seen.insert(event.as_str()).then_some(index)).collect()
}

fn initial_due(trigger: &Trigger, now_ms: u64) -> Result<Option<u64>, String> {
    match trigger {
        Trigger::Interval { seconds } => Ok(Some(interval_due(*seconds, now_ms)?)),
        Trigger::Alarm { unix_ms } => Ok(Some(*unix_ms)),
        Trigger::RoomMessages => Ok(None),
    }
}

fn interval_due(seconds: u64, now_ms: u64) -> Result<u64, String> {
    seconds.checked_mul(1000).and_then(|interval| now_ms.checked_add(interval))
        .ok_or_else(|| "Background interval exceeds the supported clock range.".into())
}

fn recover_schedule(job: &mut Job, now_ms: u64) -> Result<(), String> {
    if matches!(job.trigger, Trigger::Alarm { .. }) { job.enabled = false; job.next_due_ms = None; }
    else if job.enabled { job.next_due_ms = initial_due(&job.trigger, now_ms)?; }
    Ok(())
}

fn validate_text(value: &str, limit: usize) -> Result<(), String> {
    if value.is_empty() || value.len() > limit || value.trim() != value || value.chars().any(char::is_control) {
        Err("Background job identity is invalid.".into())
    } else { Ok(()) }
}

fn validate_binding(binding: &JobBinding) -> Result<(), String> {
    validate_text(&binding.account, 1024)?;
    validate_text(&binding.app_id, 256)?;
    if let Some(room) = binding.context.room_id() { validate_text(room, 1024)?; }
    Ok(())
}

fn validate_trigger(trigger: &Trigger, context: &JobContext) -> Result<(), String> {
    match trigger {
        Trigger::Interval { seconds } if *seconds < MIN_INTERVAL_SECONDS || seconds.checked_mul(1000).is_none() =>
            Err(format!("Background intervals must be at least {MIN_INTERVAL_SECONDS} seconds and fit the supported clock range.")),
        Trigger::RoomMessages if !matches!(context, JobContext::Room { .. }) => Err("Room-message triggers need a specific room.".into()),
        _ => Ok(()),
    }
}

fn validate_fingerprint(fingerprint: &str) -> Result<(), String> {
    if fingerprint.len() != 64 || !fingerprint.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) {
        Err("Background app fingerprint is invalid.".into())
    } else { Ok(()) }
}

fn validate_document(document: &Document) -> Result<(), String> {
    if document.schema != SCHEMA || document.jobs.len() > MAX_JOBS || document.next_job_id == 0 || document.next_run_id == 0
        || document.next_job_id > MAX_ID || document.next_run_id > MAX_ID
    {
        return Err("Background job settings have an unsupported version or invalid limits.".into());
    }
    let mut ids = BTreeSet::new();
    let mut bindings = BTreeSet::new();
    let mut run_ids = BTreeSet::new();
    let mut active = 0;
    for job in &document.jobs {
        validate_binding(&job.binding)?;
        validate_trigger(&job.trigger, &job.binding.context)?;
        validate_fingerprint(&job.fingerprint)?;
        if job.id == 0 || job.id >= document.next_job_id || !ids.insert(job.id)
            || !bindings.insert((&job.binding.account, &job.binding.app_id, job.binding.context.room_id()))
            || job.recent_events.len() > MAX_RECENT_EVENTS || (job.enabled && job.pause_reason.is_some())
            || (!job.enabled && job.next_due_ms.is_some())
            || (job.enabled && matches!(job.trigger, Trigger::Interval { .. } | Trigger::Alarm { .. }) && job.next_due_ms.is_none())
            || (matches!(job.trigger, Trigger::RoomMessages) && job.next_due_ms.is_some())
        { return Err("Background job settings contain inconsistent jobs.".into()); }
        for event in &job.recent_events { validate_text(event, 256)?; }
        if job.recent_events.iter().collect::<BTreeSet<_>>().len() != job.recent_events.len() { return Err("Background job event history is invalid.".into()); }
        if let Some(run) = &job.in_flight {
            active += 1;
            if run.run_id == 0 || run.run_id >= document.next_run_id || !run_ids.insert(run.run_id)
                || run.started_ms.checked_add(RUN_TIMEOUT_MS) != Some(run.deadline_ms)
                || run.scheduled_ms > run.started_ms || job.pause_reason.is_some()
                || (!job.enabled && !matches!(job.trigger, Trigger::Alarm { .. }))
            { return Err("Background job settings contain an invalid active run.".into()); }
        }
        if let Some(run) = &job.last_run {
            if run.run_id == 0 || run.run_id >= document.next_run_id || !run_ids.insert(run.run_id) || run.finished_ms < run.started_ms {
                return Err("Background job settings contain an invalid completed run.".into());
            }
        }
    }
    if active > MAX_CONCURRENT_RUNS { return Err("Background job settings exceed the active run limit.".into()); }
    Ok(())
}

/// Source, declarations and metadata visible to an app are reviewed together.
///
/// Version timestamps alone do not invalidate an otherwise equal app.
pub fn fingerprint(manifest: &MiniAppManifest) -> Result<String, String> {
    struct HashWriter(Sha256);
    impl Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> { self.0.update(bytes); Ok(bytes.len()) }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    let mut writer = HashWriter(Sha256::new());
    writer.0.update(b"robrix-background-app-v1\0");
    let mut permissions = manifest.permissions.iter().collect::<Vec<_>>();
    permissions.sort(); permissions.dedup();
    let mut capabilities = manifest.capabilities.iter().collect::<Vec<_>>();
    capabilities.sort(); capabilities.dedup();
    serde_json::to_writer(&mut writer, &(&manifest.id, &manifest.source, manifest.allow_net, permissions, capabilities,
        &manifest.permission_reasons, &manifest.scope, manifest.builtin, &manifest.widget,
        &manifest.name, &manifest.icon, manifest.tint, &manifest.description, &manifest.shortcuts))
        .map_err(|_| "Cannot fingerprint the installed background app.")?;
    Ok(format!("{:x}", writer.0.finalize()))
}

fn restrict_file(file: &File) -> std::io::Result<()> {
    #[cfg(unix)]
    { use std::os::unix::fs::PermissionsExt; file.set_permissions(fs::Permissions::from_mode(0o600))?; }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

fn write_document(path: &Path, document: &Document) -> Result<(), String> {
    struct BoundedBytes(Vec<u8>);
    impl Write for BoundedBytes {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > MAX_FILE_BYTES.saturating_sub(self.0.len()) { return Err(std::io::Error::other("background settings too large")); }
            self.0.extend_from_slice(bytes); Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    let mut bytes = BoundedBytes(Vec::new());
    serde_json::to_writer(&mut bytes, document).map_err(|_| "Background job settings exceed their storage limit.")?;
    let parent = path.parent().ok_or("Background job storage location is invalid.")?;
    fs::create_dir_all(parent).map_err(|_| "Cannot create background job storage.")?;
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);
    let serial = NEXT_TEMP.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| value.checked_add(1))
        .map_err(|_| "Background storage identities are exhausted.")?;
    let temporary = parent.join(format!(".background_jobs.{}.{}.tmp", std::process::id(), serial));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
    let mut file = options.open(&temporary).map_err(|_| "Cannot create a private background settings update.")?;
    let saved = (|| -> std::io::Result<()> {
        restrict_file(&file)?;
        file.write_all(&bytes.0)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if saved.is_err() { let _ = fs::remove_file(&temporary); }
    saved.map_err(|_| "Cannot save background job settings. Scheduling is paused until the settings can be reopened.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(1);
            let path = std::env::temp_dir().join(format!("robrix-background-tests-{}-{}",
                std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn open(&self, now_ms: u64) -> JobStore { JobStore::open(&self.0, now_ms).unwrap() }
        fn bytes(&self) -> Vec<u8> { fs::read(self.0.join(FILE_NAME)).unwrap() }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }

    fn hash() -> String { "a".repeat(64) }
    fn installed(_: &str) -> Option<String> { Some(hash()) }
    fn binding(app: &str) -> JobBinding {
        JobBinding { account: "@alice:example.org".into(), app_id: app.into(),
            context: JobContext::Room { room_id: "!room:example.org".into() } }
    }
    const ALICE: &str = "@alice:example.org";
    const BOB: &str = "@bob:example.org";
    fn interval() -> Trigger { Trigger::Interval { seconds: MIN_INTERVAL_SECONDS } }
    fn due(store: &mut JobStore, now_ms: u64) -> Vec<JobRun> {
        store.claim_due(ALICE, now_ms, MAX_CONCURRENT_RUNS, installed).unwrap()
    }

    #[test]
    fn intervals_are_durable_coalesced_and_nonoverlapping() {
        let root = TestRoot::new();
        let mut store = root.open(1_000);
        let id = store.enable(binding("timer"), interval(), hash(), 1_000).unwrap();
        assert_eq!(store.next_wakeup(ALICE), Some(61_000));
        assert!(due(&mut store, 60_999).is_empty());
        let run = due(&mut store, 900_000).pop().unwrap();
        assert_eq!(run.scheduled_ms, 61_000);
        assert_eq!(store.job(id).unwrap().next_due_ms, Some(960_000));
        assert!(due(&mut store, 990_000).is_empty());
        assert!(store.claim_now(ALICE, id, 990_000, installed).unwrap().is_none());
        let saved: Document = serde_json::from_slice(&root.bytes()).unwrap();
        assert_eq!(saved.jobs[0].in_flight.as_ref().unwrap().run_id, run.run_id);
        assert_eq!(store.next_wakeup(ALICE), Some(1_020_000));
        drop(store);
        let mut store = root.open(1_000_000);
        let restored = store.job(id).unwrap();
        assert!(restored.enabled);
        assert!(restored.in_flight.is_none());
        assert_eq!(restored.next_due_ms, Some(1_060_000));
        assert_eq!(restored.last_run.as_ref().unwrap().outcome, RunOutcome::Interrupted);
        assert!(due(&mut store, 1_000_000).is_empty());
        let next = due(&mut store, 1_060_000).pop().unwrap();
        assert!(next.run_id > run.run_id);
        assert!(!store.complete(ALICE, id, run.run_id, RunOutcome::Succeeded, 1_060_001).unwrap());
        assert!(store.complete(ALICE, id, next.run_id, RunOutcome::Succeeded, 1_060_001).unwrap());
    }

    #[test]
    fn completed_and_uncertain_alarms_do_not_replay_after_restart() {
        let root = TestRoot::new();
        let mut store = root.open(1_000);
        let successful = store.enable(binding("success"), Trigger::Alarm { unix_ms: 2_000 }, hash(), 1_000).unwrap();
        let uncertain = store.enable(binding("uncertain"), Trigger::Alarm { unix_ms: 2_000 }, hash(), 1_000).unwrap();
        let runs = due(&mut store, 2_000);
        assert_eq!(runs.len(), 2);
        assert!(!store.job(successful).unwrap().enabled);
        assert!(store.job(uncertain).unwrap().in_flight.is_some());
        store.complete(ALICE, successful, runs[0].run_id, RunOutcome::Succeeded, 2_100).unwrap();
        drop(store);
        let mut store = root.open(9_000);
        assert!(due(&mut store, 99_000).is_empty());
        assert_eq!(store.job(successful).unwrap().last_run.as_ref().unwrap().outcome, RunOutcome::Succeeded);
        assert_eq!(store.job(uncertain).unwrap().last_run.as_ref().unwrap().outcome, RunOutcome::Interrupted);
        assert!(store.jobs().iter().all(|job| !job.enabled && job.next_due_ms.is_none()));
        assert_eq!(store.enable(binding("uncertain"), Trigger::Alarm { unix_ms: 10_000 }, hash(), 9_000).unwrap(), uncertain);
        let resumed = due(&mut store, 10_000).pop().unwrap();
        assert!(resumed.run_id > runs[1].run_id);
    }

    #[test]
    fn unavailable_overdue_alarm_waits_without_consumption_or_busy_wakeup() {
        let root = TestRoot::new();
        let mut store = root.open(0);
        let alarm = store.enable(binding("alarm"), Trigger::Alarm { unix_ms: 10_000 }, hash(), 0).unwrap();
        let timer = store.enable(binding("timer"), interval(), hash(), 0).unwrap();
        drop(store);
        let mut store = root.open(90_000);
        let before = root.bytes();
        assert!(store.claim_due_ready(ALICE, 90_000, 4, installed, |_| false).unwrap().is_empty());
        assert_eq!(store.next_wakeup_ready(ALICE, |_| false), None);
        assert_eq!(root.bytes(), before);
        assert!(store.job(alarm).unwrap().enabled);
        assert!(store.job(alarm).unwrap().last_run.is_none());
        assert_eq!(store.job(alarm).unwrap().next_due_ms, Some(10_000));
        let runs = store.claim_due_ready(ALICE, 90_001, 4, installed, |binding| binding.app_id == "timer").unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].job_id, timer);
        assert_eq!(store.next_wakeup_ready(ALICE, |_| false), Some(runs[0].deadline_ms), "active deadlines ignore current readiness");
        let runs = store.claim_due_ready(ALICE, 90_002, 4, installed, |_| true).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].job_id, alarm);
        assert_eq!(runs[0].scheduled_ms, 10_000);
        assert!(!store.job(alarm).unwrap().enabled);
        assert_eq!(store.job(alarm).unwrap().in_flight.as_ref().unwrap().run_id, runs[0].run_id);
    }

    #[test]
    fn all_mutation_and_claim_paths_are_account_bound() {
        let root = TestRoot::new();
        let mut store = root.open(0);
        let alice = store.enable(binding("timer"), interval(), hash(), 0).unwrap();
        let mut bob = binding("timer");
        bob.account = BOB.into();
        let bob = store.enable(bob, interval(), hash(), 0).unwrap();
        assert_ne!(alice, bob);
        assert!(store.disable(BOB, alice, 0).is_err());
        assert!(store.pause(BOB, alice, PauseReason::AppChanged, 0).is_err());
        assert!(store.remove(BOB, alice).is_err());
        assert!(store.claim_now(BOB, alice, 0, installed).is_err());
        let run = due(&mut store, 60_000).pop().unwrap();
        assert_eq!(run.job_id, alice);
        assert!(!store.complete(BOB, alice, run.run_id, RunOutcome::Succeeded, 60_001).unwrap());
        assert_eq!(store.interrupt_active(BOB, 60_001).unwrap(), 0);
        let bob_runs = store.claim_due(BOB, 60_000, 4, installed).unwrap();
        assert_eq!(bob_runs.len(), 1);
        assert_eq!(bob_runs[0].job_id, bob);
        assert!(store.job(alice).unwrap().in_flight.is_some());
    }

    #[test]
    fn changed_or_missing_apps_pause_until_explicit_review() {
        let root = TestRoot::new();
        let mut store = root.open(0);
        let changed = store.enable(binding("changed"), interval(), hash(), 0).unwrap();
        let missing = store.enable(binding("missing"), interval(), hash(), 0).unwrap();
        assert!(store.claim_due(ALICE, 60_000, 4, |app| (app == "changed").then(|| "b".repeat(64))).unwrap().is_empty());
        assert_eq!(store.job(changed).unwrap().pause_reason, Some(PauseReason::AppChanged));
        assert_eq!(store.job(missing).unwrap().pause_reason, Some(PauseReason::AppMissing));
        drop(store);
        let mut store = root.open(90_000);
        assert!(due(&mut store, 120_000).is_empty());
        assert!(store.claim_now(ALICE, changed, 120_000, installed).is_err());
        assert_eq!(store.enable(binding("changed"), interval(), "b".repeat(64), 120_000).unwrap(), changed);
        assert!(store.job(changed).unwrap().pause_reason.is_none());
        assert!(store.claim_now(ALICE, changed, 120_000, |_| Some("b".repeat(64))).unwrap().is_some());
    }

    #[test]
    fn one_binding_survives_room_space_classification_changes() {
        let root = TestRoot::new();
        let mut store = root.open(0);
        let id = store.enable(binding("utility"), interval(), hash(), 0).unwrap();
        let mut space = binding("utility");
        space.context = JobContext::Space { space_id: "!room:example.org".into() };
        assert!(store.enable(space.clone(), Trigger::RoomMessages, hash(), 0).is_err());
        assert_eq!(store.enable(space.clone(), interval(), hash(), 0).unwrap(), id);
        assert_eq!(store.jobs().len(), 1);
        assert_eq!(store.job(id).unwrap().binding, space);
        let mut account = binding("utility");
        account.context = JobContext::Account;
        assert_ne!(store.enable(account, interval(), hash(), 0).unwrap(), id);
    }

    #[test]
    fn capacity_is_bounded_and_older_due_jobs_get_the_next_slot() {
        let root = TestRoot::new();
        let mut store = root.open(0);
        for index in 0..5 { store.enable(binding(&format!("timer-{index}")), interval(), hash(), 0).unwrap(); }
        let runs = due(&mut store, 60_000);
        assert_eq!(runs.len(), MAX_CONCURRENT_RUNS);
        assert_eq!(store.next_wakeup(ALICE), Some(180_000));
        assert!(due(&mut store, 120_000).is_empty());
        store.complete(ALICE, runs[0].job_id, runs[0].run_id, RunOutcome::Succeeded, 120_001).unwrap();
        assert_eq!(store.next_wakeup(ALICE), Some(60_000));
        let run = due(&mut store, 120_001).pop().unwrap();
        assert_eq!(run.binding.app_id, "timer-4");
        assert_eq!(run.scheduled_ms, 60_000);
    }

    #[test]
    fn message_jobs_deduplicate_successful_claims_and_never_queue_overlap() {
        let root = TestRoot::new();
        let mut store = root.open(0);
        let id = store.enable(binding("watcher"), Trigger::RoomMessages, hash(), 0).unwrap();
        assert!(store.claim_room_messages(ALICE, "!other:example.org", "$one", 1, 4, installed).unwrap().is_empty());
        assert!(store.claim_room_messages(BOB, "!room:example.org", "$one", 1, 4, installed).unwrap().is_empty());
        let run = store.claim_room_messages(ALICE, "!room:example.org", "$one", 1, 4, installed).unwrap().pop().unwrap();
        assert!(store.claim_room_messages(ALICE, "!room:example.org", "$two", 2, 4, installed).unwrap().is_empty());
        assert_eq!(store.job(id).unwrap().recent_events, ["$one"]);
        store.complete(ALICE, id, run.run_id, RunOutcome::Succeeded, 3).unwrap();
        assert!(store.claim_room_messages(ALICE, "!room:example.org", "$one", 4, 4, installed).unwrap().is_empty());
        drop(store);
        let mut store = root.open(100);
        assert!(due(&mut store, 100).is_empty());
        assert!(store.claim_room_messages(ALICE, "!room:example.org", "$one", 100, 4, installed).unwrap().is_empty());
        for index in 0..MAX_RECENT_EVENTS + 2 {
            let run = store.claim_room_messages(ALICE, "!room:example.org", &format!("$new-{index}"), 101, 4, installed).unwrap().pop().unwrap();
            store.complete(ALICE, id, run.run_id, RunOutcome::Succeeded, 102).unwrap();
        }
        assert_eq!(store.job(id).unwrap().recent_events.len(), MAX_RECENT_EVENTS);
        assert!(!store.job(id).unwrap().recent_events.iter().any(|event| event == "$one"));
    }

    #[test]
    fn message_batches_preserve_every_unseen_event_once_per_claimed_job() {
        let root = TestRoot::new();
        let mut store = root.open(0);
        let first = store.enable(binding("first"), Trigger::RoomMessages, hash(), 0).unwrap();
        let first_run = store.claim_room_messages(ALICE, "!room:example.org", "$seen", 1, 4, installed).unwrap().pop().unwrap();
        store.complete(ALICE, first, first_run.run_id, RunOutcome::Succeeded, 2).unwrap();
        let second = store.enable(binding("second"), Trigger::RoomMessages, hash(), 0).unwrap();
        let events = ["$seen", "$two", "$two", "$three"].map(String::from);
        let runs = store.claim_room_message_batch(ALICE, "!room:example.org", &events, 3, 4, installed).unwrap();
        assert_eq!(runs.len(), 2);
        let (_, first_indexes) = runs.iter().find(|(run, _)| run.job_id == first).unwrap();
        let (_, second_indexes) = runs.iter().find(|(run, _)| run.job_id == second).unwrap();
        assert_eq!(first_indexes, &[1, 3]);
        assert_eq!(second_indexes, &[0, 1, 3]);
        assert_eq!(store.job(first).unwrap().recent_events, ["$seen", "$two", "$three"]);
        assert_eq!(store.job(second).unwrap().recent_events, ["$seen", "$two", "$three"]);
        let busy = ["$four".to_owned(), "$five".to_owned()];
        assert!(store.claim_room_message_batch(ALICE, "!room:example.org", &busy, 4, 4, installed).unwrap().is_empty());
        assert_eq!(store.job(first).unwrap().recent_events.len(), 3);
        for (run, _) in runs { store.complete(ALICE, run.job_id, run.run_id, RunOutcome::Succeeded, 5).unwrap(); }
        assert!(store.claim_room_message_batch(ALICE, "!room:example.org", &events, 6, 4, installed).unwrap().is_empty());
        let excessive = vec!["$event".to_owned(); MAX_MESSAGE_BATCH + 1];
        assert!(store.claim_room_message_batch(ALICE, "!room:example.org", &excessive, 7, 4, installed).is_err());
        assert!(store.jobs().iter().all(|job| job.in_flight.is_none()));
    }

    #[test]
    fn retiring_runs_requires_matching_ids_and_timeout_does_not_release_work() {
        let root = TestRoot::new();
        let mut store = root.open(0);
        let id = store.enable(binding("timer"), interval(), hash(), 0).unwrap();
        let first = store.claim_now(ALICE, id, 1, installed).unwrap().unwrap();
        assert!(store.enable(binding("timer"), interval(), hash(), 1).is_err());
        assert!(store.remove(ALICE, id).is_err());
        assert!(store.expired_runs(ALICE, first.deadline_ms - 1).is_empty());
        assert_eq!(store.expired_runs(ALICE, first.deadline_ms), [first.clone()]);
        assert!(store.job(id).unwrap().in_flight.is_some());
        store.pause(ALICE, id, PauseReason::TimedOut, first.deadline_ms).unwrap();
        assert_eq!(store.job(id).unwrap().pause_reason, Some(PauseReason::TimedOut));
        assert_eq!(store.job(id).unwrap().last_run.as_ref().unwrap().outcome, RunOutcome::Interrupted);
        store.enable(binding("timer"), interval(), hash(), 200_000).unwrap();
        let second = store.claim_now(ALICE, id, 200_000, installed).unwrap().unwrap();
        assert!(!store.complete(ALICE, id, first.run_id, RunOutcome::Succeeded, 200_001).unwrap());
        assert_eq!(store.job(id).unwrap().in_flight.as_ref().unwrap().run_id, second.run_id);
        store.disable(ALICE, id, 200_002).unwrap();
        assert_eq!(store.job(id).unwrap().last_run.as_ref().unwrap().outcome, RunOutcome::Cancelled);
        assert!(!store.complete(ALICE, id, second.run_id, RunOutcome::Succeeded, 200_003).unwrap());
        store.remove(ALICE, id).unwrap();
        assert!(store.job(id).is_none());
    }

    #[test]
    fn interruption_preserves_desired_interval_and_message_state_only() {
        let root = TestRoot::new();
        let mut store = root.open(0);
        let timer = store.enable(binding("timer"), interval(), hash(), 0).unwrap();
        let message = store.enable(binding("watcher"), Trigger::RoomMessages, hash(), 0).unwrap();
        let alarm = store.enable(binding("alarm"), Trigger::Alarm { unix_ms: 100 }, hash(), 0).unwrap();
        for id in [timer, message, alarm] { store.claim_now(ALICE, id, 1, installed).unwrap().unwrap(); }
        assert_eq!(store.interrupt_active(ALICE, 5).unwrap(), 3);
        assert_eq!(store.interrupt_active(ALICE, 5).unwrap(), 0);
        assert_eq!(store.job(timer).unwrap().next_due_ms, Some(60_005));
        assert!(store.job(message).unwrap().enabled);
        assert_eq!(store.job(message).unwrap().next_due_ms, None);
        assert!(!store.job(alarm).unwrap().enabled);
        assert!(store.jobs().iter().all(|job| job.last_run.as_ref().unwrap().outcome == RunOutcome::Interrupted));
    }

    #[test]
    fn invalid_configuration_and_job_limit_leave_saved_jobs_unchanged() {
        let root = TestRoot::new();
        let mut store = root.open(0);
        assert!(store.enable(binding("short"), Trigger::Interval { seconds: 59 }, hash(), 0).is_err());
        assert!(store.enable(binding("overflow"), Trigger::Interval { seconds: u64::MAX }, hash(), 0).is_err());
        assert!(store.enable(binding("hash"), interval(), "not-a-hash".into(), 0).is_err());
        let mut account = binding("account");
        account.context = JobContext::Account;
        assert!(store.enable(account, Trigger::RoomMessages, hash(), 0).is_err());
        for index in 0..MAX_JOBS { store.enable(binding(&format!("timer-{index}")), interval(), hash(), 0).unwrap(); }
        let bytes = root.bytes();
        assert!(store.enable(binding("excess"), interval(), hash(), 0).is_err());
        assert_eq!(root.bytes(), bytes);
        assert_eq!(store.jobs().len(), MAX_JOBS);
        assert!(store.enable(binding("timer-0"), interval(), hash(), 1).is_ok());
    }

    #[test]
    fn corruption_never_overwrites_or_enables_settings() {
        let root = TestRoot::new();
        let path = root.0.join(FILE_NAME);
        for invalid in [b"not JSON".to_vec(), b"{}".to_vec(), vec![b' '; MAX_FILE_BYTES + 1]] {
            fs::write(&path, &invalid).unwrap();
            assert!(JobStore::open(&root.0, 0).is_err());
            assert_eq!(fs::read(&path).unwrap(), invalid);
        }
        fs::remove_file(&path).unwrap();
        let mut store = root.open(0);
        store.enable(binding("timer"), interval(), hash(), 0).unwrap();
        let valid = store.document.clone();
        drop(store);
        let mut invalid_documents = Vec::new();
        let mut invalid = valid.clone();
        invalid.next_job_id = MAX_ID + 1;
        invalid_documents.push(invalid);
        let mut invalid = valid.clone();
        invalid.jobs[0].enabled = false;
        invalid_documents.push(invalid);
        let mut invalid = valid.clone();
        invalid.schema += 1;
        invalid_documents.push(invalid);
        let mut invalid = valid;
        let mut duplicate = invalid.jobs[0].clone();
        duplicate.id = invalid.next_job_id;
        duplicate.binding.context = JobContext::Space { space_id: "!room:example.org".into() };
        invalid.next_job_id += 1;
        invalid.jobs.push(duplicate);
        invalid_documents.push(invalid);
        for invalid in invalid_documents {
            let bytes = serde_json::to_vec(&invalid).unwrap();
            fs::write(&path, &bytes).unwrap();
            assert!(JobStore::open(&root.0, 0).is_err());
            assert_eq!(root.bytes(), bytes);
        }
    }

    #[test]
    fn clock_and_identity_overflow_are_atomic_and_do_not_dispatch() {
        let root = TestRoot::new();
        let mut store = root.open(0);
        let id = store.enable(binding("timer"), interval(), hash(), 0).unwrap();
        let bytes = root.bytes();
        assert!(store.enable(binding("overflow"), interval(), hash(), u64::MAX).is_err());
        assert!(store.claim_now(ALICE, id, u64::MAX - 1, installed).is_err());
        assert_eq!(root.bytes(), bytes);
        assert!(store.job(id).unwrap().in_flight.is_none());
        let mut exhausted = store.document.clone();
        exhausted.next_run_id = MAX_ID;
        exhausted.next_job_id = MAX_ID;
        store.persist(exhausted).unwrap();
        let bytes = root.bytes();
        assert!(store.enable(binding("new"), interval(), hash(), 0).is_err());
        assert!(store.claim_now(ALICE, id, 1, installed).is_err());
        assert_eq!(root.bytes(), bytes);
        assert_eq!(store.error(), None);
    }

    #[test]
    fn failed_writes_poison_dispatch_without_replacing_in_memory_state() {
        let root = TestRoot::new();
        let mut store = root.open(0);
        let id = store.enable(binding("timer"), interval(), hash(), 0).unwrap();
        let bytes = root.bytes();
        let path = root.0.join(FILE_NAME);
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(store.claim_now(ALICE, id, 1, installed).is_err());
        assert!(store.error().is_some());
        assert!(store.job(id).unwrap().in_flight.is_none());
        assert!(store.next_wakeup(ALICE).is_none());
        fs::remove_dir(&path).unwrap();
        fs::write(&path, bytes).unwrap();
        assert!(store.claim_now(ALICE, id, 1, installed).is_err());
        drop(store);
        let mut reopened = root.open(2);
        assert!(reopened.claim_now(ALICE, id, 2, installed).unwrap().is_some());
    }

    #[test]
    fn scheduler_ownership_is_exclusive_and_settings_are_private() {
        let root = TestRoot::new();
        let mut store = root.open(0);
        assert!(JobStore::open(&root.0, 0).is_err());
        store.enable(binding("timer"), interval(), hash(), 0).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(root.0.join(FILE_NAME)).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(fs::metadata(root.0.join(".background_jobs.lock")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        drop(store);
        assert!(JobStore::open(&root.0, 1).is_ok());
    }

    #[test]
    fn fingerprint_covers_source_identity_scope_and_security_contract() {
        let manifest: MiniAppManifest = serde_json::from_value(serde_json::json!({
            "id": "timer", "name": "Timer", "icon": "clock", "tint": 1, "source": "// background: true\nView{}",
            "allow_net": false, "builtin": false, "widget": null,
            "permissions": ["network", "room.read"], "capabilities": ["network.fetch"]
        })).unwrap();
        let original = fingerprint(&manifest).unwrap();
        assert_eq!(original.len(), 64);
        let mut reordered = manifest.clone();
        reordered.permissions.reverse();
        reordered.permissions.push(reordered.permissions[0].clone());
        reordered.current_version = Some("new timestamp".into());
        assert_eq!(fingerprint(&reordered).unwrap(), original);
        let mut source = manifest.clone();
        source.source.push_str("\n// changed");
        let mut identity = manifest.clone();
        identity.name = "Another app".into();
        let mut scope = manifest.clone();
        scope.scope = crate::manifest::A2AppScope::Room { room_id: "!different:example.org".into() };
        let mut contract = manifest.clone();
        contract.capabilities.push("room.read_messages".into());
        let mut provenance = manifest;
        provenance.builtin = true;
        for changed in [source, identity, scope, contract, provenance] {
            assert_ne!(fingerprint(&changed).unwrap(), original);
        }
    }
}
