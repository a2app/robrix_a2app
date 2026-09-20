//! Bounded, local protection history for the trusted host UI.
//!
//! Records contain identities and outcomes, never request bodies, full URLs,
//! credentials, model replies, or user-reviewed action payloads. This history
//! is diagnostic, not an authorization store or a promise of remote delivery.

use std::{collections::VecDeque, fs::{self, File, OpenOptions}, io::{BufRead, BufReader, Read, Write}, path::{Path, PathBuf}, sync::{Mutex, OnceLock}, time::{SystemTime, UNIX_EPOCH}};
use serde::{Deserialize, Serialize};
use crate::information_flow::{ContextId, Label, Recipient};

const MAX_EVENTS: usize = 256;
const MAX_RECORD_BYTES: usize = 64 * 1024;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ActivityKind { SharingCheck, ModelRequest, HttpRequest, MatrixOperation, ToolCall, PolicyChange }

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ActivityOutcome { Allowed, Blocked, Started, Completed, Failed, Interrupted, Changed }

impl ActivityOutcome {
    pub fn label(self) -> &'static str {
        match self {
            Self::Allowed => "Policy allowed; no transmission implied",
            Self::Blocked => "Blocked by sharing policy",
            Self::Started => "Request started; delivery unconfirmed",
            Self::Completed => "Response received / operation completed",
            Self::Failed => "Request failed; some data may already have left",
            Self::Interrupted => "Request interrupted; some data may already have left",
            Self::Changed => "Protection settings changed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Activity {
    pub id: u64,
    pub timestamp_ms: u64,
    pub account: String,
    pub context: Option<ContextId>,
    pub recipient: Option<Recipient>,
    pub sources: Label,
    pub blocked_sources: Label,
    pub kind: ActivityKind,
    pub outcome: ActivityOutcome,
}

impl Activity {
    fn validate(&self) -> bool {
        if self.account.is_empty() || self.account.len() > 4096
            || self.context.as_ref().is_some_and(|context| context.account() != self.account)
            || self.sources.len() > 256 || self.blocked_sources.len() > 256
        { return false; }
        match &self.recipient {
            Some(Recipient::NetworkOrigin(origin)) => Recipient::network_origin(origin).is_ok_and(|recipient| Some(recipient) == self.recipient),
            _ => true,
        }
    }

    fn same_decision(&self, other: &Self) -> bool {
        self.account == other.account && self.context == other.context && self.recipient == other.recipient
            && self.sources == other.sources && self.blocked_sources == other.blocked_sources
            && self.kind == other.kind && self.outcome == other.outcome
    }
}

struct History {
    path: PathBuf,
    entries: VecDeque<Activity>,
    file: Option<File>,
    bytes: u64,
    next_id: u64,
    revision: u64,
    warning: Option<String>,
}

impl History {
    fn open(root: &Path) -> Self {
        let path = root.join("protection_history.jsonl");
        let mut history = Self { path, entries: VecDeque::new(), file: None, bytes: 0, next_id: 1, revision: 1, warning: None };
        let result = (|| -> std::io::Result<()> {
            let file = match File::open(&history.path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            };
            restrict_file(&file)?;
            history.bytes = file.metadata()?.len();
            if history.bytes > MAX_FILE_BYTES { return Err(std::io::Error::other("oversized protection history")); }
            let mut reader = BufReader::new(file.take(MAX_FILE_BYTES + 1));
            let mut read_bytes = 0;
            loop {
                let mut line = Vec::new();
                let count = reader.by_ref().take(MAX_RECORD_BYTES as u64 + 1).read_until(b'\n', &mut line)?;
                if count == 0 { break; }
                read_bytes += count as u64;
                if read_bytes > MAX_FILE_BYTES || line.len() > MAX_RECORD_BYTES {
                    return Err(std::io::Error::other("oversized history record"));
                }
                if line.last() != Some(&b'\n') { return Err(std::io::Error::other("incomplete history record")); }
                line.pop();
                if line.is_empty() { continue; }
                let entry: Activity = serde_json::from_slice(&line).map_err(std::io::Error::other)?;
                if !entry.validate() || entry.id < history.next_id { return Err(std::io::Error::other("invalid history record")); }
                history.next_id = entry.id.checked_add(1).ok_or_else(|| std::io::Error::other("history identities exhausted"))?;
                history.push(entry);
            }
            history.bytes = read_bytes;
            Ok(())
        })();
        history.revision = history.next_id;
        if result.is_err() {
            history.warn("Protection history is incomplete or unreadable. Current permissions and IFC enforcement are unaffected.");
        }
        history
    }

    fn push(&mut self, entry: Activity) {
        if self.entries.len() == MAX_EVENTS { self.entries.pop_front(); }
        self.entries.push_back(entry);
    }

    fn warn(&mut self, message: &str) {
        if self.warning.as_deref() != Some(message) {
            self.warning = Some(message.into());
            self.revision = self.revision.saturating_add(1);
        }
    }

    fn record(&mut self, mut entry: Activity) {
        if !entry.validate() { self.warn("A protection history record exceeded its metadata limits."); return; }
        // In-flight transports recheck policy frequently. Keep one adjacent
        // identical check rather than writing a record every 100 milliseconds.
        if entry.kind == ActivityKind::SharingCheck && self.entries.back().is_some_and(|last| last.same_decision(&entry)) { return; }
        entry.id = self.next_id;
        let Some(next) = self.next_id.checked_add(1) else { self.warn("Protection history identities exhausted."); return; };
        entry.timestamp_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis().min(u64::MAX as u128) as u64;
        let Ok(mut bytes) = serde_json::to_vec(&entry) else { return; };
        if bytes.len() >= MAX_RECORD_BYTES { self.warn("A protection history record exceeded its metadata limits."); return; }
        bytes.push(b'\n');
        self.next_id = next;
        self.revision = self.revision.saturating_add(1);
        self.push(entry);
        if self.warning.is_some() { return; }
        let result = (|| -> std::io::Result<()> {
            let parent = self.path.parent().expect("history has a parent");
            fs::create_dir_all(parent)?;
            if self.bytes + bytes.len() as u64 > MAX_FILE_BYTES {
                self.file = None;
                let temporary = self.path.with_extension("jsonl.tmp");
                let mut file = private_file(&temporary, false)?;
                let mut size = 0;
                // Retain a byte-bounded suffix even with unusually large labels.
                let mut retained = Vec::new();
                for entry in self.entries.iter().rev() {
                    let mut line = serde_json::to_vec(entry).map_err(std::io::Error::other)?;
                    line.push(b'\n');
                    if size + line.len() as u64 > MAX_FILE_BYTES / 2 { break; }
                    size += line.len() as u64;
                    retained.push(line);
                }
                for line in retained.iter().rev() { file.write_all(line)?; }
                file.sync_all()?;
                fs::rename(temporary, &self.path)?;
                self.bytes = size;
                return Ok(());
            }
            if self.file.is_none() { self.file = Some(private_file(&self.path, true)?); }
            self.file.as_mut().expect("opened above").write_all(&bytes)?;
            self.bytes += bytes.len() as u64;
            Ok(())
        })();
        if result.is_err() { self.file = None; self.warn("Protection history could not be saved. Current permissions and IFC enforcement are unaffected."); }
    }
}

fn private_file(path: &Path, append: bool) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).write(true).append(append).truncate(!append);
    #[cfg(unix)]
    { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
    let file = options.open(path)?;
    restrict_file(&file)?;
    Ok(file)
}

fn restrict_file(file: &File) -> std::io::Result<()> {
    #[cfg(unix)]
    { use std::os::unix::fs::PermissionsExt; file.set_permissions(fs::Permissions::from_mode(0o600))?; }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

static HISTORY: OnceLock<Mutex<History>> = OnceLock::new();

fn with_history<T>(f: impl FnOnce(&mut History) -> T) -> Result<T, String> {
    HISTORY.get_or_init(|| Mutex::new(History::open(crate::data_root()))).lock()
        .map(|mut history| f(&mut history)).map_err(|_| "Protection history is unavailable.".into())
}

pub fn recent(account: &str) -> Result<(Vec<Activity>, Option<String>), String> {
    with_history(|history| (history.entries.iter().rev().filter(|entry| entry.account == account).cloned().collect(), history.warning.clone()))
}

pub fn revision() -> u64 { with_history(|history| history.revision).unwrap_or_default() }

pub fn record_sharing(context: &ContextId, recipient: &Recipient, sources: &Label, blocked_sources: &Label) {
    let entry = Activity { id: 0, timestamp_ms: 0, account: context.account().into(), context: Some(context.clone()),
        recipient: Some(recipient.clone()), sources: sources.clone(), blocked_sources: blocked_sources.clone(),
        kind: ActivityKind::SharingCheck, outcome: if blocked_sources.is_empty() { ActivityOutcome::Allowed } else { ActivityOutcome::Blocked } };
    let _ = with_history(|history| history.record(entry));
}

pub fn record_policy_change(account: &str) {
    let entry = Activity { id: 0, timestamp_ms: 0, account: account.into(), context: None, recipient: None,
        sources: Label::new(), blocked_sources: Label::new(), kind: ActivityKind::PolicyChange, outcome: ActivityOutcome::Changed };
    let _ = with_history(|history| history.record(entry));
}

/// An actual transport attempt. Dropping an unfinished attempt records an
/// interruption; it never claims cancellation recalled data already sent.
pub struct Attempt { entry: Activity, finished: bool }

impl Attempt {
    pub fn start(context: &ContextId, recipient: Option<Recipient>, kind: ActivityKind) -> Self {
        let entry = Activity { id: 0, timestamp_ms: 0, account: context.account().into(), context: Some(context.clone()), recipient,
            sources: crate::information_flow::labels(context).unwrap_or_else(|_| [crate::information_flow::Source::UnknownPrivate].into_iter().collect()),
            blocked_sources: Label::new(), kind, outcome: ActivityOutcome::Started };
        let _ = with_history(|history| history.record(entry.clone()));
        Self { entry, finished: false }
    }

    pub fn finish(mut self, succeeded: bool) {
        self.entry.outcome = if succeeded { ActivityOutcome::Completed } else { ActivityOutcome::Failed };
        let _ = with_history(|history| history.record(self.entry.clone()));
        self.finished = true;
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        if !self.finished {
            self.entry.outcome = ActivityOutcome::Interrupted;
            let _ = with_history(|history| history.record(self.entry.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Root(PathBuf);
    impl Root {
        fn new() -> Self { Self(std::env::temp_dir().join(format!("robrix_audit_{}_{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)))) }
    }
    impl Drop for Root { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
    fn entry() -> Activity {
        Activity { id: 0, timestamp_ms: 0, account: "@owner:test".into(), context: None, recipient: None,
            sources: Label::new(), blocked_sources: Label::new(), kind: ActivityKind::PolicyChange, outcome: ActivityOutcome::Changed }
    }
    #[test]
    fn history_survives_restart_and_bounds_retention() {
        let root = Root::new();
        let mut history = History::open(&root.0);
        for _ in 0..300 { history.record(entry()); }
        let restored = History::open(&root.0);
        assert!(restored.warning.is_none());
        assert_eq!(restored.entries.len(), MAX_EVENTS);
        assert_eq!(restored.entries.front().unwrap().id, 45);
        assert_eq!(restored.entries.back().unwrap().id, 300);
        assert!(restored.entries.back().unwrap().timestamp_ms > 0);
    }
    #[test]
    fn repeated_checks_are_coalesced_and_full_urls_are_rejected() {
        let root = Root::new();
        let mut history = History::open(&root.0);
        let mut check = entry();
        check.kind = ActivityKind::SharingCheck;
        for _ in 0..20 { history.record(check.clone()); }
        assert_eq!(history.entries.len(), 1);
        check.recipient = Some(Recipient::NetworkOrigin("https://example.test/private?secret=value".into()));
        history.record(check);
        assert_eq!(history.entries.len(), 1);
        assert!(history.warning.is_some());
    }
    #[test]
    fn corrupt_history_is_reported_without_disabling_enforcement() {
        let root = Root::new();
        fs::create_dir_all(&root.0).unwrap();
        fs::write(root.0.join("protection_history.jsonl"), b"incomplete record").unwrap();
        let mut history = History::open(&root.0);
        assert!(history.warning.is_some());
        history.record(entry());
        assert_eq!(history.entries.len(), 1, "new metadata is still visible in this run");
    }

    #[test]
    fn warning_without_a_record_advances_the_ui_revision() {
        let root = Root::new();
        let mut history = History::open(&root.0);
        let before = history.revision;
        let mut invalid = entry();
        invalid.account.clear();
        history.record(invalid);
        assert!(history.entries.is_empty());
        assert!(history.warning.is_some());
        assert!(history.revision > before);
    }

    #[test]
    fn oversized_records_and_unterminated_tails_are_not_appended_to() {
        let root = Root::new();
        fs::create_dir_all(&root.0).unwrap();
        let path = root.0.join("protection_history.jsonl");
        for bytes in [vec![b' '; MAX_RECORD_BYTES + 100], serde_json::to_vec(&entry()).unwrap()] {
            fs::write(&path, &bytes).unwrap();
            let mut history = History::open(&root.0);
            assert!(history.warning.is_some());
            assert!(history.entries.is_empty());
            history.record(entry());
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn disk_rotation_preserves_the_newest_complete_bounded_suffix() {
        let root = Root::new();
        let mut history = History::open(&root.0);
        let mut activity = entry();
        activity.account = "a".repeat(4096);
        for _ in 0..600 { history.record(activity.clone()); }
        assert!(history.warning.is_none());
        assert!(fs::metadata(&history.path).unwrap().len() <= MAX_FILE_BYTES);
        drop(history);
        let restored = History::open(&root.0);
        assert!(restored.warning.is_none());
        assert!(restored.entries.len() <= MAX_EVENTS);
        assert_eq!(restored.entries.back().unwrap().id, 600);
        assert!(restored.entries.front().unwrap().id > 1);
    }

    #[cfg(unix)]
    #[test]
    fn existing_history_files_are_kept_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = Root::new();
        fs::create_dir_all(&root.0).unwrap();
        let path = root.0.join("protection_history.jsonl");
        fs::write(&path, b"").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        let mut history = History::open(&root.0);
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        history.record(entry());
        assert!(history.warning.is_none());
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
