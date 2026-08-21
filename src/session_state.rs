//! Per-session read tracking (Milestone 5): one JSON file per harness session
//! under `state_dir/sessions/`, keyed by session id so concurrent sessions
//! never cross-contaminate each other's dedupe state.
//!
//! Correctness envelope for the repeat-read advisory (a nagging hook gets
//! disabled, so every guard here exists to keep the advisory honest):
//! - mtime gates every warning — a changed file is a legitimate re-read;
//! - writes clear the read record — post-edit content is new content;
//! - at most one advisory per file per session;
//! - compaction disarms warnings — the model lost the earlier read;
//! - subagents are never warned (enforced by the hook layer: they run in
//!   fresh context where the "earlier content" does not exist).
//!
//! Retention is enforced at the writer: every store GCs session files older
//! than seven days, so the directory never needs a cleanup job.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Session files untouched for longer than this are dropped on the next write.
const GC_MAX_AGE_SECS: u64 = 7 * 24 * 3600;

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
}

impl SessionState {
    pub fn record_read(&mut self, path: &str, mtime: u64) {
        self.reads.insert(path.to_string(), ReadRecord { mtime, at: now() });
    }

    /// A write invalidates the read record: the earlier content is stale, so
    /// the next read is legitimate and must not warn.
    pub fn record_write(&mut self, path: &str) {
        self.reads.remove(path);
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
