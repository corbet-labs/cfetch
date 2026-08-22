//! Per-session read tracking (Milestone 5): one JSON file per harness session
//! under `state_dir/sessions/`, keyed by session id so concurrent sessions
//! never cross-contaminate each other's dedupe state.
//!
//! Correctness envelope for the repeat-read advisory (a nagging hook gets
//! disabled, so every guard here exists to keep the advisory honest):
//! - mtime gates every warning — a changed file is a legitimate re-read;
//! - writes clear the read record — post-edit content is new content;
//! - at most one advisory per file per session;
//! - compaction disarms warnings — the model lost the earlier read, and
//!   [`SessionState::written`] is what the hook layer hands back in their
//!   place, so this file outlives the conversation on purpose;
//! - subagents are never warned (enforced by the hook layer: they run in
//!   fresh context where the "earlier content" does not exist);
//! - only a start that begins a NEW conversation clears the file — see
//!   [`StartKind`], which turns the SessionStart reason into that decision.
//!
//! Retention is enforced at the writer: every store GCs session files older
//! than seven days, so the directory never needs a cleanup job.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Session files untouched for longer than this are dropped on the next write.
const GC_MAX_AGE_SECS: u64 = 7 * 24 * 3600;
/// Cross-platform atomic replacement is slower on macOS and Windows CI; this
/// still stays well below the command-hook timeout while preventing a short
/// burst of same-session hooks from dropping state updates.
const UPDATE_LOCK_WAIT_MS: u64 = 2_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadRecord {
    /// File mtime (unix seconds) at the moment it was read.
    pub mtime: u64,
    /// When the read happened (unix seconds).
    pub at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SessionState {
    /// Set by PreCompact: the model's memory of earlier reads is gone, so
    /// repeat-read warnings are disarmed for the rest of the session.
    #[serde(default)]
    pub compacted: bool,
    /// Full-file reads this session: path -> record. Ranged reads are never
    /// recorded (they must not mark a later full read as duplicate).
    #[serde(default)]
    pub reads: BTreeMap<String, ReadRecord>,
    /// Files already warned about — the once-per-file-per-session cap.
    #[serde(default)]
    pub warned: BTreeSet<String>,
    /// Files already given symbol-slice hints — same once-per-session cap.
    #[serde(default)]
    pub hinted: BTreeSet<String>,
    /// Files written this session (governance: the stale-STATUS nudge needs
    /// to know what the session touched, not just what it read).
    #[serde(default)]
    pub written: BTreeSet<String>,
    /// Reminders queued at Stop (or by the cadence counter), delivered at the
    /// next UserPromptSubmit: (key, text). A Stop-level injection forces a
    /// whole extra model turn; a queued one rides the next prompt for free.
    #[serde(default)]
    pub queued_reminders: Vec<(String, String)>,
    /// Reminder keys already delivered — at most once per session per key.
    #[serde(default)]
    pub shown_keys: BTreeSet<String>,
    /// Post-tool events seen this session, for cadence re-injection.
    #[serde(default)]
    pub tool_events: u64,
    /// Writes performed by a shell command rather than a write tool. COUNTED,
    /// never named: a redirect's target is only reliably knowable for the
    /// simplest commands, and a half-parsed path asserted as fact is worse
    /// than an honest tally. Activity truth flows up, paths do not — without
    /// this, a session that edits only through the shell looks idle and every
    /// activity-gated reminder stays silent.
    #[serde(default)]
    pub shell_writes: u64,
}

impl SessionState {
    pub fn record_read(&mut self, path: &str, mtime: u64) {
        self.reads.insert(path.to_string(), ReadRecord { mtime, at: now() });
    }

    /// A write invalidates the read record: the earlier content is stale, so
    /// the next read is legitimate and must not warn. The path also joins the
    /// session's written set (the stale-STATUS nudge reads it at Stop).
    pub fn record_write(&mut self, path: &str) {
        self.reads.remove(path);
        self.written.insert(path.to_string());
    }

    /// One shell-performed write. The path is deliberately not taken.
    pub fn record_shell_write(&mut self) {
        self.shell_writes = self.shell_writes.saturating_add(1);
    }

    /// Decides — and books — a repeat-read advisory for `path`. Returns true
    /// at most once per file per session, only while the file is unchanged
    /// since it was read, and never after compaction.
    pub fn should_warn_repeat_read(&mut self, path: &str, current_mtime: u64) -> bool {
        if self.compacted || self.warned.contains(path) {
            return false;
        }
        let Some(rec) = self.reads.get(path) else { return false };
        if rec.mtime != current_mtime {
            return false;
        }
        self.warned.insert(path.to_string());
        true
    }

    /// Books a symbol-slice hint for `path`; true at most once per session.
    pub fn should_hint_slices(&mut self, path: &str) -> bool {
        self.hinted.insert(path.to_string())
    }

    /// Queues a reminder for the next UserPromptSubmit, deduplicated by key:
    /// a key already queued or already delivered this session queues nothing.
    /// Returns whether the reminder was queued.
    pub fn queue_reminder(&mut self, key: &str, text: &str) -> bool {
        if self.shown_keys.contains(key) || self.queued_reminders.iter().any(|(k, _)| k == key) {
            return false;
        }
        self.queued_reminders.push((key.to_string(), text.to_string()));
        true
    }

    /// Empties the queue and marks every drained key as shown, returning the
    /// reminder texts in queue order.
    pub fn drain_reminders(&mut self) -> Vec<String> {
        std::mem::take(&mut self.queued_reminders)
            .into_iter()
            .map(|(key, text)| {
                self.shown_keys.insert(key);
                text
            })
            .collect()
    }

    /// Counts one post-tool event; returns `Some(count)` exactly when this
    /// event is the `every`-th one (cadence re-injection is due). `every` of
    /// zero never fires — it is the cadence off switch.
    pub fn count_tool_event(&mut self, every: u32) -> Option<u64> {
        self.tool_events += 1;
        (every > 0 && self.tool_events.is_multiple_of(u64::from(every)))
            .then_some(self.tool_events)
    }

    /// Drops everything that describes the conversation, keeping the session
    /// file itself. Called only for a start that begins a NEW conversation
    /// under an id the harness may have reused — see [`StartKind`].
    ///
    /// Deliberately scoped to this file: the ledger and the exhaust stream are
    /// facts of record — what a session spent and what it did happened, and no
    /// start reason unhappens it.
    ///
    /// Written as a full destructure so that a field added to `SessionState`
    /// stops compiling here until someone decides whether the new state
    /// belongs to the conversation (cleared) or to the session id (kept).
    pub fn reset(&mut self) {
        let Self {
            compacted,
            reads,
            warned,
            hinted,
            written,
            queued_reminders,
            shown_keys,
            tool_events,
            shell_writes,
        } = self;
        *compacted = false;
        reads.clear();
        warned.clear();
        hinted.clear();
        written.clear();
        queued_reminders.clear();
        shown_keys.clear();
        *tool_events = 0;
        *shell_writes = 0;
    }
}

/// The SessionStart reason, reduced to the one question this file has to
/// answer: does the arriving conversation still remember what the records
/// describe?
///
/// It has to be asked, because the session id does not answer it. The harness
/// keeps one id across compaction and resume, so the file outlives the
/// conversation it was written for; without the reason, a resumed session
/// either forgets what it read or re-spends every once-per-session cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartKind {
    /// A fresh conversation, possibly under a recycled session id.
    Startup,
    /// The context was thrown away in place (`/clear`).
    Clear,
    /// The earlier turns are back in context.
    Resume,
    /// The conversation continues, summarized.
    Compact,
    /// A reason this build has no rule for.
    Unknown,
}

impl StartKind {
    /// Lenient parse. The harness already renamed the field once
    /// (SessionStart `source` -> `reason`), so its values are assumed to drift
    /// too: case and surrounding space never decide behaviour, and the plain
    /// morphological variants of the four known reasons are accepted. Anything
    /// else is `Unknown` — an unrecognized word is reported as unrecognized,
    /// never mapped to the nearest guess.
    pub fn parse(reason: &str) -> StartKind {
        match reason.trim().to_ascii_lowercase().as_str() {
            "startup" | "start" | "new" => StartKind::Startup,
            "clear" | "cleared" | "reset" => StartKind::Clear,
            "resume" | "resumed" | "continue" => StartKind::Resume,
            "compact" | "compaction" | "compacted" => StartKind::Compact,
            _ => StartKind::Unknown,
        }
    }

    /// Whether this start begins a conversation that remembers nothing of what
    /// the session file records.
    ///
    /// RESET on `startup` and `clear`: the context is empty, so every read
    /// record, written path and spent once-per-session cap belongs to a
    /// conversation that no longer exists. Keeping them would deny the new
    /// conversation advisories it has never seen, and warn it about reads it
    /// never made.
    ///
    /// CONTINUE on `resume` and `compact`: the conversation goes on. Resume
    /// puts the earlier turns back in context, compaction keeps them in
    /// summarized form; the caps those turns spent are spent. Compaction in
    /// particular must continue, or the reset would erase the `compacted` flag
    /// PreCompact just set and re-arm the warnings compaction disarmed.
    ///
    /// CONTINUE on an unknown reason, because continuing is the recoverable
    /// branch. A stale record can only produce advice the existing guards
    /// already bound — mtime-gated, once per file, disarmed by compaction —
    /// while a wrong reset is irreversible: it drops queued reminders and
    /// re-arms every once-per-session cap, and a hook that repeats itself is a
    /// hook the operator turns off.
    pub fn resets_state(self) -> bool {
        matches!(self, StartKind::Startup | StartKind::Clear)
    }

    /// The cause named in the re-injection note, for the starts where memory
    /// arrives mid-conversation and the model is owed an explanation for it.
    /// `None` where there is nothing to explain (a fresh conversation) or
    /// nothing we can state as fact (an unknown reason).
    pub fn continuation_label(self) -> Option<&'static str> {
        match self {
            StartKind::Resume => Some("resume"),
            StartKind::Compact => Some("compact"),
            StartKind::Startup | StartKind::Clear | StartKind::Unknown => None,
        }
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// File mtime in unix seconds; `None` when the file cannot be statted.
pub fn file_mtime(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

fn sessions_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("sessions")
}

/// Session ids come from the harness and are used as file names — anything
/// outside a conservative charset is replaced so a hostile or malformed id
/// cannot escape the sessions directory.
fn sanitize(session_id: &str) -> String {
    let s: String = session_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' })
        .collect();
    if s.is_empty() { "unknown-session".to_string() } else { s }
}

pub fn file_for(state_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir(state_dir).join(format!("{}.json", sanitize(session_id)))
}

/// Loads the session state; any failure yields a fresh state — losing dedupe
/// history degrades to a missed advisory, never a broken hook.
pub fn load(state_dir: &Path, session_id: &str) -> SessionState {
    std::fs::read_to_string(file_for(state_dir, session_id))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Stores the session state (atomic tmp+rename) and GCs stale siblings in
/// the same call — retention enforced at the writer. Best-effort throughout.
pub fn store(state_dir: &Path, session_id: &str, state: &SessionState) {
    let path = file_for(state_dir, session_id);
    let dir = sessions_dir(state_dir);
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(s) = serde_json::to_string_pretty(state) {
        // Unique tmp per writer so concurrent hooks cannot rename a torn
        // file into place (same rule as the ledger).
        let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
    gc(&dir, &path);
}

/// Lock-safe read/modify/write for hook processes that share one session.
/// Contention is bounded: a missed advisory or cadence tick is preferable to
/// stalling the agent or overwriting another hook's newer state.
pub fn update<R>(
    state_dir: &Path,
    session_id: &str,
    mutate: impl FnOnce(&mut SessionState) -> R,
) -> Option<R> {
    let dir = sessions_dir(state_dir);
    std::fs::create_dir_all(&dir).ok()?;
    let lock_path = dir.join(format!("{}.lock", sanitize(session_id)));
    let _lock = crate::lockfile::acquire(&lock_path, UPDATE_LOCK_WAIT_MS, 0)?;
    let mut state = load(state_dir, session_id);
    let result = mutate(&mut state);
    store(state_dir, session_id, &state);
    Some(result)
}

fn gc(dir: &Path, keep: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let now = SystemTime::now();
    for entry in rd.flatten() {
        let p = entry.path();
        if p == keep || p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let stale = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age.as_secs() > GC_MAX_AGE_SECS);
        if stale {
            let _ = std::fs::remove_file(&p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeat_read_advisory_fires_once_with_unchanged_mtime() {
        let mut st = SessionState::default();
        assert!(!st.should_warn_repeat_read("/a.rs", 100), "first read never warns");
        st.record_read("/a.rs", 100);
        assert!(st.should_warn_repeat_read("/a.rs", 100), "unchanged repeat read warns");
        assert!(!st.should_warn_repeat_read("/a.rs", 100), "at most once per file per session");
    }

    #[test]
    fn mtime_change_escapes_the_warning() {
        let mut st = SessionState::default();
        st.record_read("/a.rs", 100);
        assert!(!st.should_warn_repeat_read("/a.rs", 200), "changed file = legitimate re-read");
        // The old record stays until a fresh read replaces it, and the
        // escape did not consume the once-per-file budget.
        st.record_read("/a.rs", 200);
        assert!(st.should_warn_repeat_read("/a.rs", 200));
    }

    #[test]
    fn concurrent_updates_do_not_lose_counters() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::sync::Arc::new(dir.path().to_path_buf());
        let workers: Vec<_> = (0..24)
            .map(|_| {
                let root = root.clone();
                std::thread::spawn(move || {
                    update(&root, "shared", |state| {
                        state.tool_events += 1;
                    })
                    .expect("lock acquired");
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(load(dir.path(), "shared").tool_events, 24);
    }

    #[test]
    fn write_clears_the_read_record() {
        let mut st = SessionState::default();
        st.record_read("/a.rs", 100);
        st.record_write("/a.rs");
        assert!(!st.should_warn_repeat_read("/a.rs", 100), "post-write re-read is legitimate");
    }

    #[test]
    fn compaction_disarms_warnings() {
        let mut st = SessionState::default();
        st.record_read("/a.rs", 100);
        st.compacted = true;
        assert!(!st.should_warn_repeat_read("/a.rs", 100), "the model lost the earlier read");
    }

    #[test]
    fn slice_hints_fire_once_per_file() {
        let mut st = SessionState::default();
        assert!(st.should_hint_slices("/big.rs"));
        assert!(!st.should_hint_slices("/big.rs"));
        assert!(st.should_hint_slices("/other.rs"));
    }

    #[test]
    fn writes_are_recorded_for_the_status_nudge() {
        let mut st = SessionState::default();
        st.record_write("/b/agents/knowledge/topic.md");
        st.record_write("/b/agents/knowledge/topic.md");
        st.record_write("/elsewhere/other.md");
        assert!(st.written.contains("/b/agents/knowledge/topic.md"));
        assert!(st.written.contains("/elsewhere/other.md"));
        assert_eq!(st.written.len(), 2, "written paths are a set, not a log");
    }

    #[test]
    fn reminder_queue_dedups_by_key() {
        let mut st = SessionState::default();
        assert!(st.queue_reminder("status", "first"));
        assert!(!st.queue_reminder("status", "second"), "key already queued");
        assert!(st.queue_reminder("staging", "other"));
        assert_eq!(st.drain_reminders(), vec!["first".to_string(), "other".to_string()]);
        assert!(!st.queue_reminder("status", "third"), "a shown key never re-queues");
        assert!(st.drain_reminders().is_empty());
    }

    #[test]
    fn drain_empties_the_queue_and_marks_keys_shown() {
        let mut st = SessionState::default();
        assert!(st.queue_reminder("k1", "t1"));
        assert!(st.queue_reminder("k2", "t2"));
        assert_eq!(st.drain_reminders(), vec!["t1".to_string(), "t2".to_string()]);
        assert!(st.queued_reminders.is_empty());
        assert!(st.shown_keys.contains("k1") && st.shown_keys.contains("k2"));
        assert!(st.drain_reminders().is_empty(), "draining an empty queue yields nothing");
    }

    #[test]
    fn tool_event_cadence_fires_at_exactly_n_and_2n() {
        let mut st = SessionState::default();
        let mut fired = Vec::new();
        for _ in 0..7 {
            if let Some(n) = st.count_tool_event(3) {
                fired.push(n);
            }
        }
        assert_eq!(fired, vec![3, 6], "cadence must fire at exactly N and 2N");
        assert_eq!(st.tool_events, 7);
    }

    #[test]
    fn cadence_of_zero_never_fires() {
        let mut st = SessionState::default();
        for _ in 0..5 {
            assert_eq!(st.count_tool_event(0), None);
        }
        assert_eq!(st.tool_events, 5, "the counter still advances");
    }

    #[test]
    fn governance_state_survives_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = SessionState::default();
        st.record_write("/b/agents/x.md");
        assert!(st.queue_reminder("status", "update the STATUS"));
        st.shown_keys.insert("staging".into());
        st.tool_events = 24;
        store(dir.path(), "s1", &st);
        let mut back = load(dir.path(), "s1");
        assert!(back.written.contains("/b/agents/x.md"));
        assert_eq!(back.tool_events, 24);
        assert!(!back.queue_reminder("staging", "dup"), "shown keys survive the roundtrip");
        assert_eq!(back.drain_reminders(), vec!["update the STATUS".to_string()]);
    }

    #[test]
    fn only_a_new_conversation_resets_the_session_file() {
        assert!(StartKind::parse("startup").resets_state());
        assert!(StartKind::parse("clear").resets_state());
        assert!(!StartKind::parse("resume").resets_state(), "resume continues the conversation");
        assert!(!StartKind::parse("compact").resets_state(), "compaction continues it too");
        assert_eq!(StartKind::parse("resume").continuation_label(), Some("resume"));
        assert_eq!(StartKind::parse("compact").continuation_label(), Some("compact"));
        assert_eq!(StartKind::parse("startup").continuation_label(), None);
    }

    #[test]
    fn an_unknown_reason_takes_the_non_destructive_branch() {
        let kind = StartKind::parse("teleported");
        assert_eq!(kind, StartKind::Unknown, "no nearest-guess mapping");
        assert!(!kind.resets_state(), "a reason we cannot read must never destroy state");
        assert_eq!(kind.continuation_label(), None, "no cause we cannot name is asserted");
    }

    #[test]
    fn reason_parsing_absorbs_case_and_spacing_drift() {
        assert_eq!(StartKind::parse(" Resume\n"), StartKind::Resume);
        assert_eq!(StartKind::parse("COMPACT"), StartKind::Compact);
        assert_eq!(StartKind::parse("Startup"), StartKind::Startup);
        assert_eq!(StartKind::parse(""), StartKind::Unknown);
    }

    #[test]
    fn reset_drops_every_conversation_record_and_frees_the_caps() {
        let mut st = SessionState::default();
        st.record_read("/a.rs", 100);
        assert!(st.should_warn_repeat_read("/a.rs", 100));
        assert!(st.should_hint_slices("/a.rs"));
        st.record_write("/b.rs");
        st.record_shell_write();
        st.count_tool_event(1);
        assert!(st.queue_reminder("status", "nudge"));
        st.drain_reminders();
        st.compacted = true;

        st.reset();

        assert!(st.reads.is_empty() && st.written.is_empty());
        assert!(st.queued_reminders.is_empty());
        assert_eq!((st.tool_events, st.shell_writes), (0, 0));
        assert!(!st.compacted, "a fresh conversation has not been compacted");
        // The caps belong to the conversation, so the new one gets them whole.
        st.record_read("/a.rs", 100);
        assert!(st.should_warn_repeat_read("/a.rs", 100), "the warned cap is free again");
        assert!(st.should_hint_slices("/a.rs"), "the hint cap is free again");
        assert!(st.queue_reminder("status", "nudge"), "the shown-key cap is free again");
    }

    #[test]
    fn store_load_roundtrip_preserves_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = SessionState::default();
        st.record_read("/a.rs", 42);
        st.warned.insert("/a.rs".into());
        st.compacted = true;
        store(dir.path(), "s1", &st);
        let back = load(dir.path(), "s1");
        assert_eq!(back.reads["/a.rs"].mtime, 42);
        assert!(back.warned.contains("/a.rs"));
        assert!(back.compacted);
        // A different session sees nothing.
        assert!(load(dir.path(), "s2").reads.is_empty());
    }

    #[test]
    fn corrupt_state_file_degrades_to_fresh_state() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(sessions_dir(dir.path())).unwrap();
        std::fs::write(file_for(dir.path(), "s1"), "{ not json").unwrap();
        assert!(load(dir.path(), "s1").reads.is_empty());
    }

    #[test]
    fn session_files_stay_inside_the_sessions_dir() {
        let dir = tempfile::tempdir().unwrap();
        let p = file_for(dir.path(), "../../etc/passwd");
        assert_eq!(p.parent().unwrap(), sessions_dir(dir.path()), "no path traversal");
        let empty = file_for(dir.path(), "");
        assert_eq!(empty.file_name().unwrap(), "unknown-session.json");
    }

    #[test]
    fn gc_removes_session_files_older_than_seven_days_on_write() {
        let dir = tempfile::tempdir().unwrap();
        store(dir.path(), "old-session", &SessionState::default());
        let old_path = file_for(dir.path(), "old-session");
        assert!(old_path.exists());
        let f = std::fs::OpenOptions::new().write(true).open(&old_path).unwrap();
        f.set_modified(SystemTime::now() - std::time::Duration::from_secs(8 * 24 * 3600))
            .unwrap();
        drop(f);
        // A recent file must survive the same GC pass.
        store(dir.path(), "fresh-session", &SessionState::default());
        store(dir.path(), "new-session", &SessionState::default());
        assert!(!old_path.exists(), "old session file must be GC'd on write");
        assert!(file_for(dir.path(), "fresh-session").exists());
        assert!(file_for(dir.path(), "new-session").exists());
    }
}
