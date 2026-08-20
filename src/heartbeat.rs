//! Proof of life for invisible infrastructure. Every hook invocation records
//! its outcome; session-start surfaces a degradation banner when any hook is
//! failing repeatedly. "No findings" and "no data" are different things.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct HookHealth {
    pub last_ok: Option<u64>,
    pub last_error: Option<String>,
    pub last_error_at: Option<u64>,
    pub consecutive_failures: u32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Heartbeat {
    #[serde(default)]
    pub hooks: BTreeMap<String, HookHealth>,
}

fn file_in(state_dir: &std::path::Path) -> PathBuf {
    state_dir.join("heartbeat.json")
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn load_from(state_dir: &std::path::Path) -> Heartbeat {
    std::fs::read_to_string(file_in(state_dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Best-effort: heartbeat writing must never fail a hook.
fn store(state_dir: &std::path::Path, hb: &Heartbeat) {
    let path = file_in(state_dir);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(s) = serde_json::to_string_pretty(hb) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

pub fn record_ok(hook: &str) {
    record_ok_in(&paths::state_dir(), hook)
}

pub fn record_ok_in(state_dir: &std::path::Path, hook: &str) {
    let mut hb = load_from(state_dir);
    let h = hb.hooks.entry(hook.to_string()).or_default();
    h.last_ok = Some(now());
    h.consecutive_failures = 0;
    store(state_dir, &hb);
}

pub fn record_error(hook: &str, err: &str) {
    record_error_in(&paths::state_dir(), hook, err)
}

pub fn record_error_in(state_dir: &std::path::Path, hook: &str, err: &str) {
    let mut hb = load_from(state_dir);
    let h = hb.hooks.entry(hook.to_string()).or_default();
    h.last_error = Some(err.chars().take(500).collect());
    h.last_error_at = Some(now());
    h.consecutive_failures = h.consecutive_failures.saturating_add(1);
    store(state_dir, &hb);
}

/// Hooks that have failed 3+ times in a row — the degradation banner input.
pub fn degraded() -> Vec<(String, HookHealth)> {
    degraded_in(&paths::state_dir())
}

pub fn degraded_in(state_dir: &std::path::Path) -> Vec<(String, HookHealth)> {
    load_from(state_dir)
        .hooks
        .into_iter()
        .filter(|(_, h)| h.consecutive_failures >= 3)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_accumulate_and_ok_resets() {
        let dir = tempfile::tempdir().unwrap();
        record_error_in(dir.path(), "stop", "boom");
        record_error_in(dir.path(), "stop", "boom");
        record_error_in(dir.path(), "stop", "boom");
        assert_eq!(degraded_in(dir.path()).len(), 1);
        record_ok_in(dir.path(), "stop");
        assert!(degraded_in(dir.path()).is_empty());
        let hb = load_from(dir.path());
        assert_eq!(hb.hooks["stop"].consecutive_failures, 0);
        assert!(hb.hooks["stop"].last_error.is_some());
    }
}
