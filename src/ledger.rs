//! Injection booking: every character cfetch itself puts into a session is
//! recorded per source. A memory system that only counts what it saves and
//! never what it costs lies about its own value. Retention is enforced at the
//! writer — a cap only enforced by a cleanup job is not a cap.
//!
//! Milestone 5 adds MEASURED usage from the session transcript, booked with
//! per-turn delta semantics: Stop fires once per TURN and the transcript
//! counters are cumulative for the session, so each booking records only
//! `max(0, current - booked)` per metric and advances the booked watermark in
//! the same locked write. Booking cumulative counters directly would inflate
//! totals 1+2+...+N-fold (upstream shipped exactly that bug: 898M fake
//! tokens). A counter that shrinks (transcript reset/truncated) clamps the
//! delta at zero and re-arms from the lower watermark.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SessionInjections {
    pub started_at: u64,
    /// source label -> (injections, chars, estimated tokens)
    #[serde(default)]
    pub by_source: BTreeMap<String, SourceTotals>,
    /// Sum of per-turn booked deltas — the honest measured total.
    #[serde(default)]
    pub measured: MeasuredUsage,
    /// Cumulative transcript counters as of the last booking (the watermark).
    #[serde(default)]
    pub booked: MeasuredUsage,
}

/// Token usage measured from the transcript — ground truth, kept strictly
/// apart from the estimated injection counters.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeasuredUsage {
    pub api_calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

impl MeasuredUsage {
    pub fn is_zero(&self) -> bool {
        *self == MeasuredUsage::default()
    }

    pub fn accumulate(&mut self, other: &MeasuredUsage) {
        self.api_calls += other.api_calls;
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
    }
}

impl From<&crate::transcript::TranscriptUsage> for MeasuredUsage {
    fn from(u: &crate::transcript::TranscriptUsage) -> MeasuredUsage {
        MeasuredUsage {
            api_calls: u.api_calls,
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_input_tokens: u.cache_read_input_tokens,
            cache_creation_input_tokens: u.cache_creation_input_tokens,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SourceTotals {
    pub count: u64,
    pub chars: u64,
    pub tokens_estimated: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Ledger {
    /// session_id -> injections. BTreeMap for deterministic serialization.
    #[serde(default)]
    pub sessions: BTreeMap<String, SessionInjections>,
}

fn file_in(state_dir: &std::path::Path) -> PathBuf {
    state_dir.join("ledger.json")
}

/// Ledger lock: ~500ms max wait, 5s stale-steal. `None` means proceed
/// UNLOCKED — a clobbered counter beats a stalled hook.
fn acquire_lock(state_dir: &std::path::Path) -> Option<crate::lockfile::Lock> {
    crate::lockfile::acquire(&state_dir.join("ledger.lock"), 500, 5)
}

/// A corrupt ledger is moved aside (bytes preserved for forensics), never
/// overwritten in place and never allowed to permanently wedge booking. At
/// most 3 quarantine files are kept.
fn quarantine(state_dir: &std::path::Path) {
    let path = file_in(state_dir);
    let ts = now();
    let _ = std::fs::rename(&path, state_dir.join(format!("ledger.json.corrupt-{ts}")));
    if let Ok(rd) = std::fs::read_dir(state_dir) {
        let mut quarantined: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("ledger.json.corrupt-"))
            })
            .collect();
        quarantined.sort();
        while quarantined.len() > 3 {
            let _ = std::fs::remove_file(quarantined.remove(0));
        }
    }
}

/// Number of quarantined corrupt ledgers — surfaced by status/selfcheck so a
/// torn write is visible instead of silent.
pub fn quarantine_count(state_dir: &std::path::Path) -> usize {
    std::fs::read_dir(state_dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.starts_with("ledger.json.corrupt-"))
                })
                .count()
        })
        .unwrap_or(0)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn load() -> Ledger {
    load_from(&paths::state_dir())
}

/// A corrupt ledger is NOT replaced with a stub — losing history to save one
/// counter is the wrong trade. book() refuses to write in that case.
pub fn load_from(state_dir: &std::path::Path) -> Ledger {
    std::fs::read_to_string(file_in(state_dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn corrupt(state_dir: &std::path::Path) -> bool {
    match std::fs::read_to_string(file_in(state_dir)) {
        Ok(s) => serde_json::from_str::<Ledger>(&s).is_err(),
        Err(_) => false,
    }
}

/// Records an injection. Best-effort; never fails the calling hook.
pub fn book(session_id: &str, source: &str, chars: usize, max_sessions: usize) {
    book_in(&paths::state_dir(), session_id, source, chars, max_sessions)
}

pub fn book_in(
    state_dir: &std::path::Path,
    session_id: &str,
    source: &str,
    chars: usize,
    max_sessions: usize,
) {
    if chars == 0 {
        return;
    }
    let _ = std::fs::create_dir_all(state_dir);
    let _lock = acquire_lock(state_dir);
    if corrupt(state_dir) {
        quarantine(state_dir);
    }
    let mut ledger = load_from(state_dir);
    let s = ledger
        .sessions
        .entry(session_id.to_string())
        .or_insert_with(|| SessionInjections { started_at: now(), ..Default::default() });
    let t = s.by_source.entry(source.to_string()).or_default();
    t.count += 1;
    t.chars += chars as u64;
    t.tokens_estimated += crate::hook_io::estimate_tokens(chars);

    retain_newest(&mut ledger, max_sessions);
    persist(state_dir, &ledger);
}

/// Books measured transcript usage for one turn. `current` is CUMULATIVE for
/// the session (what `transcript::scan` returns); only the delta above the
/// watermark is added, and the watermark advances in the same locked write.
/// Best-effort; never fails the calling hook.
pub fn book_measured(session_id: &str, current: &MeasuredUsage, max_sessions: usize) {
    book_measured_in(&paths::state_dir(), session_id, current, max_sessions)
}

pub fn book_measured_in(
    state_dir: &std::path::Path,
    session_id: &str,
    current: &MeasuredUsage,
    max_sessions: usize,
) {
    let _ = std::fs::create_dir_all(state_dir);
    let _lock = acquire_lock(state_dir);
    if corrupt(state_dir) {
        quarantine(state_dir);
    }
    let mut ledger = load_from(state_dir);
    let s = ledger
        .sessions
        .entry(session_id.to_string())
        .or_insert_with(|| SessionInjections { started_at: now(), ..Default::default() });

    // max(0, current - booked) per metric: a shrunken counter books nothing
    // (clamp), and the watermark still moves to `current` so growth after a
    // reset is booked from the new baseline.
    s.measured.api_calls += current.api_calls.saturating_sub(s.booked.api_calls);
    s.measured.input_tokens += current.input_tokens.saturating_sub(s.booked.input_tokens);
    s.measured.output_tokens += current.output_tokens.saturating_sub(s.booked.output_tokens);
    s.measured.cache_read_input_tokens +=
        current.cache_read_input_tokens.saturating_sub(s.booked.cache_read_input_tokens);
    s.measured.cache_creation_input_tokens +=
        current.cache_creation_input_tokens.saturating_sub(s.booked.cache_creation_input_tokens);
    s.booked = current.clone();

    retain_newest(&mut ledger, max_sessions);
    persist(state_dir, &ledger);
}

/// Writer-side retention: keep the newest max_sessions by started_at.
fn retain_newest(ledger: &mut Ledger, max_sessions: usize) {
    if ledger.sessions.len() > max_sessions.max(1) {
        let mut by_age: Vec<(String, u64)> =
            ledger.sessions.iter().map(|(k, v)| (k.clone(), v.started_at)).collect();
        by_age.sort_by_key(|(_, at)| *at);
        let drop = ledger.sessions.len() - max_sessions.max(1);
        for (k, _) in by_age.into_iter().take(drop) {
            ledger.sessions.remove(&k);
        }
    }
}

fn persist(state_dir: &std::path::Path, ledger: &Ledger) {
    let path = file_in(state_dir);
    if let Ok(s) = serde_json::to_string_pretty(ledger) {
        // Unique tmp per writer: a shared tmp name lets two processes
        // interleave open/truncate/write and rename a torn file into place.
        let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn booking_accumulates_per_source() {
        let dir = tempfile::tempdir().unwrap();
        book_in(dir.path(), "s1", "resident", 700, 10);
        book_in(dir.path(), "s1", "resident", 300, 10);
        book_in(dir.path(), "s1", "banner", 35, 10);
        let l = load_from(dir.path());
        let s = &l.sessions["s1"];
        assert_eq!(s.by_source["resident"].count, 2);
        assert_eq!(s.by_source["resident"].chars, 1000);
        assert!(s.by_source["resident"].tokens_estimated >= 285);
        assert_eq!(s.by_source["banner"].count, 1);
    }

    #[test]
    fn retention_caps_at_writer() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..8 {
            book_in(dir.path(), &format!("s{i}"), "resident", 10, 5);
        }
        let l = load_from(dir.path());
        assert!(l.sessions.len() <= 5, "kept {}", l.sessions.len());
    }

    #[test]
    fn corrupt_ledger_is_quarantined_and_booking_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = file_in(dir.path());
        std::fs::write(&path, "{ not json").unwrap();
        book_in(dir.path(), "s1", "resident", 100, 10);
        // The corrupt bytes are preserved aside, not overwritten in place…
        assert_eq!(quarantine_count(dir.path()), 1);
        let q: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_str().unwrap().starts_with("ledger.json.corrupt-"))
            .collect();
        assert_eq!(std::fs::read_to_string(q[0].path()).unwrap(), "{ not json");
        // …and booking works again instead of wedging forever.
        let l = load_from(dir.path());
        assert_eq!(l.sessions["s1"].by_source["resident"].chars, 100);
    }

    #[test]
    fn lock_is_released_after_booking() {
        // The lock file is PERMANENT (flock guards it; the fd is the lock) —
        // release is proven by the next booking succeeding, and an old
        // unheld file must not block booking either.
        let dir = tempfile::tempdir().unwrap();
        book_in(dir.path(), "s1", "resident", 10, 10);
        let lock = dir.path().join("ledger.lock");
        assert!(lock.exists(), "the lock file stays on disk after release");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        let f = std::fs::OpenOptions::new().write(true).open(&lock).unwrap();
        f.set_modified(old).unwrap();
        drop(f);
        book_in(dir.path(), "s2", "resident", 10, 10);
        assert_eq!(load_from(dir.path()).sessions.len(), 2);
    }

    #[test]
    fn zero_chars_books_nothing() {
        let dir = tempfile::tempdir().unwrap();
        book_in(dir.path(), "s1", "resident", 0, 10);
        assert!(load_from(dir.path()).sessions.is_empty());
    }

    fn usage(calls: u64, inp: u64, out: u64, cr: u64, cc: u64) -> MeasuredUsage {
        MeasuredUsage {
            api_calls: calls,
            input_tokens: inp,
            output_tokens: out,
            cache_read_input_tokens: cr,
            cache_creation_input_tokens: cc,
        }
    }

    #[test]
    fn measured_booking_records_per_turn_deltas_not_cumulative_sums() {
        let dir = tempfile::tempdir().unwrap();
        let t1 = usage(2, 100, 50, 10, 5);
        book_measured_in(dir.path(), "s1", &t1, 10);
        let s1 = load_from(dir.path()).sessions["s1"].clone();
        assert_eq!(s1.measured, t1, "first booking takes the full cumulative value");
        assert_eq!(s1.booked, t1, "watermark advances in the same write");

        // Turn 2 reports CUMULATIVE counters again — booking must add only
        // the delta, so the total equals the cumulative value, not 1+2 sums.
        let t2 = usage(3, 160, 80, 40, 5);
        book_measured_in(dir.path(), "s1", &t2, 10);
        let s2 = load_from(dir.path()).sessions["s1"].clone();
        assert_eq!(s2.measured, t2, "sum of booked deltas == cumulative counters");
        assert_eq!(s2.booked, t2);
    }

    #[test]
    fn measured_reset_to_zero_clamps_and_rearms_from_new_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let t1 = usage(2, 100, 50, 10, 5);
        book_measured_in(dir.path(), "s1", &t1, 10);

        // Counters shrink (transcript reset/truncated): book nothing, but
        // the watermark must follow DOWN so later growth books again.
        book_measured_in(dir.path(), "s1", &MeasuredUsage::default(), 10);
        let s = load_from(dir.path()).sessions["s1"].clone();
        assert_eq!(s.measured, t1, "clamp at zero: a shrunken counter books no negative delta");
        assert!(s.booked.is_zero(), "watermark re-arms at the lower value");

        book_measured_in(dir.path(), "s1", &usage(1, 40, 8, 0, 0), 10);
        let s = load_from(dir.path()).sessions["s1"].clone();
        assert_eq!(s.measured, usage(3, 140, 58, 10, 5), "post-reset growth is booked in full");
    }

    #[test]
    fn measured_and_injection_booking_share_a_session_entry() {
        let dir = tempfile::tempdir().unwrap();
        book_in(dir.path(), "s1", "resident", 100, 10);
        book_measured_in(dir.path(), "s1", &usage(1, 10, 5, 0, 0), 10);
        let l = load_from(dir.path());
        assert_eq!(l.sessions.len(), 1);
        let s = &l.sessions["s1"];
        assert_eq!(s.by_source["resident"].chars, 100);
        assert_eq!(s.measured.input_tokens, 10);
    }

    #[test]
    fn old_ledgers_without_measured_fields_still_load() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            file_in(dir.path()),
            r#"{"sessions":{"s1":{"started_at":1,"by_source":{"resident":{"count":1,"chars":10,"tokens_estimated":3}}}}}"#,
        )
        .unwrap();
        let l = load_from(dir.path());
        assert!(l.sessions["s1"].measured.is_zero());
        // …and measured booking on top of the old shape works.
        book_measured_in(dir.path(), "s1", &usage(1, 7, 3, 0, 0), 10);
        assert_eq!(load_from(dir.path()).sessions["s1"].measured.input_tokens, 7);
    }
}
