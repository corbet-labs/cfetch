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
    if chars == 0 || corrupt(state_dir) {
        return;
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
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(s) = serde_json::to_string_pretty(&ledger) {
        let tmp = path.with_extension("json.tmp");
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
    fn corrupt_ledger_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = file_in(dir.path());
        std::fs::write(&path, "{ not json").unwrap();
        book_in(dir.path(), "s1", "resident", 100, 10);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw, "{ not json");
    }

    #[test]
    fn zero_chars_books_nothing() {
        let dir = tempfile::tempdir().unwrap();
        book_in(dir.path(), "s1", "resident", 0, 10);
        assert!(load_from(dir.path()).sessions.is_empty());
    }
}
