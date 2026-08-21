//! Injection booking: every character cfetch itself puts into a session is
//! recorded per source. A memory system that only counts what it saves and
//! never what it costs lies about its own value. Retention is enforced at the
//! writer — a cap only enforced by a cleanup job is not a cap.

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
        .or_insert_with(|| SessionInjections { started_at: now(), by_source: BTreeMap::new() });
    let t = s.by_source.entry(source.to_string()).or_default();
    t.count += 1;
    t.chars += chars as u64;
    t.tokens_estimated += crate::hook_io::estimate_tokens(chars);

    // Writer-side retention: keep the newest max_sessions by started_at.
    if ledger.sessions.len() > max_sessions.max(1) {
        let mut by_age: Vec<(String, u64)> =
            ledger.sessions.iter().map(|(k, v)| (k.clone(), v.started_at)).collect();
        by_age.sort_by_key(|(_, at)| *at);
        let drop = ledger.sessions.len() - max_sessions.max(1);
        for (k, _) in by_age.into_iter().take(drop) {
            ledger.sessions.remove(&k);
        }
    }

    let path = file_in(state_dir);
    if let Ok(s) = serde_json::to_string_pretty(&ledger) {
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
    fn lock_is_released_and_stale_locks_are_stolen() {
        let dir = tempfile::tempdir().unwrap();
        book_in(dir.path(), "s1", "resident", 10, 10);
        assert!(!dir.path().join("ledger.lock").exists(), "lock must be released");
        // A stale lock (old mtime) must not block booking.
        let lock = dir.path().join("ledger.lock");
        std::fs::write(&lock, "999999").unwrap();
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
}
