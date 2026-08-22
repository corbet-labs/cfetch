//! Proof of life for invisible infrastructure — and for cfetch's own
//! measurements. Every hook invocation records its outcome and every look at
//! the derived index records what it saw, so that a reporting surface can
//! tell "measured, and found nothing" apart from "never measured at all".
//!
//! A failure counter alone cannot make that distinction: a hook that has
//! never fired and a hook that has run cleanly all day both leave zero
//! failures behind. The EXPECTED set below is the missing half — without a
//! list of what should have reported, an absent record is invisible, and a
//! dead measurement renders exactly like a clean one. cfetch spent its whole
//! life verifying transcript delivery against harness fields that do not
//! exist, and nothing noticed, because a check that found nothing looked
//! identical to a check that found nothing wrong.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::paths;

/// The hook subcommands `cfetch install` registers with every harness that
/// supports the full lifecycle (`install::FULL_HOOKS`). This list is the
/// expectation a liveness check is measured against; nothing else on disk
/// remembers that a hook was supposed to run, so without it an absent record
/// cannot be told from a healthy one.
pub const REGISTERED_HOOKS: &[&str] =
    &["session-start", "user-prompt", "pre-tool", "post-tool", "stop", "precompact"];

/// Consecutive failures before a hook is loud enough for the degradation
/// banner.
pub const DEGRADED_AFTER: u32 = 3;

/// How long a derived index may lag the tree before its age is worth a
/// warning. Every host refreshes on a 60s backstop, so an index still behind
/// an hour later has a broken refresh path, not a busy one.
pub const STALE_INDEX_WARN_SECS: u64 = 3600;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct HookHealth {
    pub last_ok: Option<u64>,
    pub last_error: Option<String>,
    pub last_error_at: Option<u64>,
    pub consecutive_failures: u32,
}

/// One recorded look at the derived index: what the committed catalog claimed
/// about the tree, what the tree actually looked like, and when the two first
/// disagreed. The onset is written down because it cannot be recovered after
/// the fact — the catalog carries no build timestamp, so a "stale" without a
/// `stale_since` can never grow into "stale for nine days".
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct IndexObservation {
    /// `None` = no catalog has ever been committed on this host.
    pub index_fingerprint: Option<String>,
    pub tree_fingerprint: String,
    /// First moment this host saw the two disagree; cleared once they agree.
    pub stale_since: Option<u64>,
    pub observed_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Heartbeat {
    #[serde(default)]
    pub hooks: BTreeMap<String, HookHealth>,
    #[serde(default)]
    pub index: Option<IndexObservation>,
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
        .filter(|(_, h)| h.consecutive_failures >= DEGRADED_AFTER)
        .collect()
}

/// What the heartbeat can honestly say about one hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookState {
    /// Registered, but carrying no record at all: this hook has never run on
    /// this host. It is not healthy, it is unobserved, and every figure it
    /// feeds is unproven.
    Unobserved,
    /// Last invocation succeeded.
    Healthy { last_ok: u64 },
    /// Last invocation failed.
    Failing { consecutive: u32, last_error: Option<String>, last_ok: Option<u64> },
}

impl HookState {
    pub fn is_degraded(&self) -> bool {
        matches!(self, HookState::Failing { consecutive, .. } if *consecutive >= DEGRADED_AFTER)
    }

    fn of(h: &HookHealth) -> HookState {
        if h.consecutive_failures > 0 {
            return HookState::Failing {
                consecutive: h.consecutive_failures,
                last_error: h.last_error.clone(),
                last_ok: h.last_ok,
            };
        }
        // A record carrying neither a success nor a failure proves nothing
        // about the hook, so it counts as silence rather than as health.
        match h.last_ok {
            Some(last_ok) => HookState::Healthy { last_ok },
            None => HookState::Unobserved,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HookLiveness {
    pub name: String,
    /// False for a record left by a hook cfetch no longer registers. Reported
    /// rather than folded into the counts: a stale name means the expectation
    /// and the installation have drifted apart.
    pub registered: bool,
    pub state: HookState,
}

/// How loudly a surface should render a liveness picture. Deliberately free
/// of any rendering type, so the TUI, the CLI and the daemon all key their
/// colours and wording off one verdict instead of three thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Healthy,
    Unobserved,
    Failing,
}

/// Expected versus observed for every hook cfetch registers.
#[derive(Debug, Clone)]
pub struct Liveness {
    pub hooks: Vec<HookLiveness>,
}

impl Liveness {
    /// Registered hooks that have never reported. Not a subset of the failing
    /// ones — these are the hooks no measurement covers at all.
    pub fn unobserved(&self) -> Vec<&str> {
        self.hooks
            .iter()
            .filter(|h| h.registered && h.state == HookState::Unobserved)
            .map(|h| h.name.as_str())
            .collect()
    }

    pub fn degraded(&self) -> Vec<&HookLiveness> {
        self.hooks.iter().filter(|h| h.state.is_degraded()).collect()
    }

    pub fn registered_count(&self) -> usize {
        self.hooks.iter().filter(|h| h.registered).count()
    }

    /// True when not one hook has ever reported. The strongest statement this
    /// module makes: cfetch cannot see itself on this host at all.
    pub fn blind(&self) -> bool {
        self.hooks.iter().all(|h| h.state == HookState::Unobserved)
    }

    pub fn severity(&self) -> Severity {
        if !self.degraded().is_empty() {
            Severity::Failing
        } else if self.blind() || !self.unobserved().is_empty() {
            Severity::Unobserved
        } else {
            Severity::Healthy
        }
    }

    /// One line, phrased so no state can be mistaken for another. The
    /// forbidden sentence is "all healthy" while part of the set is silent.
    pub fn summary(&self) -> String {
        let registered = self.registered_count();
        let mut parts = Vec::new();
        if self.blind() {
            parts.push(format!(
                "hooks: NEVER OBSERVED — none of the {registered} registered hook(s) has reported on this host; health is unknown, not healthy"
            ));
        } else {
            let degraded = self.degraded();
            if !degraded.is_empty() {
                let names: Vec<String> = degraded
                    .iter()
                    .map(|h| match &h.state {
                        HookState::Failing { consecutive, .. } => {
                            format!("{} ({consecutive}×)", h.name)
                        }
                        _ => h.name.clone(),
                    })
                    .collect();
                parts.push(format!("hooks FAILING: {}", names.join(", ")));
            }
            let unobserved = self.unobserved();
            if !unobserved.is_empty() {
                parts.push(format!(
                    "{} of {registered} registered hook(s) reporting; NEVER OBSERVED: {}",
                    registered - unobserved.len(),
                    unobserved.join(", ")
                ));
            } else if degraded.is_empty() {
                parts
                    .push(format!("hooks: all {registered} registered hook(s) reporting, healthy"));
            }
        }
        let unregistered: Vec<&str> =
            self.hooks.iter().filter(|h| !h.registered).map(|h| h.name.as_str()).collect();
        if !unregistered.is_empty() {
            parts.push(format!("unregistered record(s): {}", unregistered.join(", ")));
        }
        let mut line = parts.join(" — ");
        if !line.starts_with("hooks") {
            line.insert_str(0, "hooks: ");
        }
        line
    }
}

pub fn liveness() -> Liveness {
    liveness_in(&paths::state_dir())
}

/// Folds the recorded heartbeat against [`REGISTERED_HOOKS`]. Every
/// registered hook appears in the result whether or not it left a record —
/// that is the whole point: absence has to be representable to be reported.
pub fn liveness_in(state_dir: &std::path::Path) -> Liveness {
    let hb = load_from(state_dir);
    let mut hooks: Vec<HookLiveness> = REGISTERED_HOOKS
        .iter()
        .map(|name| HookLiveness {
            name: (*name).to_string(),
            registered: true,
            state: hb.hooks.get(*name).map(HookState::of).unwrap_or(HookState::Unobserved),
        })
        .collect();
    for (name, h) in &hb.hooks {
        if !REGISTERED_HOOKS.contains(&name.as_str()) {
            hooks.push(HookLiveness {
                name: name.clone(),
                registered: false,
                state: HookState::of(h),
            });
        }
    }
    Liveness { hooks }
}

/// What the derived index is worth as a source of numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexLiveness {
    /// No catalog has ever been committed here. Its counts are not zeros;
    /// they are the absence of a measurement.
    NeverScanned,
    /// The committed catalog describes the tree as it is now.
    Current,
    /// The catalog describes an older tree, and has done so for this long.
    Stale { stale_for: u64 },
}

impl IndexLiveness {
    pub fn severity(&self) -> Severity {
        match self {
            IndexLiveness::NeverScanned => Severity::Unobserved,
            IndexLiveness::Current => Severity::Healthy,
            IndexLiveness::Stale { stale_for } => {
                if *stale_for >= STALE_INDEX_WARN_SECS {
                    Severity::Failing
                } else {
                    Severity::Unobserved
                }
            }
        }
    }

    pub fn describe(&self) -> String {
        match self {
            IndexLiveness::NeverScanned => {
                "index: NEVER SCANNED on this host — its counts would be an absence of data, not zeros"
                    .to_string()
            }
            IndexLiveness::Current => {
                "index: current — the catalog's fingerprint matches the tree".to_string()
            }
            IndexLiveness::Stale { stale_for } if *stale_for >= STALE_INDEX_WARN_SECS => format!(
                "index: STALE for {} — the catalog describes a tree that old, and the refresh path has not caught up",
                human_age(*stale_for)
            ),
            IndexLiveness::Stale { stale_for } => {
                format!("index: stale for {} — a refresh is pending", human_age(*stale_for))
            }
        }
    }
}

/// Records one look at the index and returns what it means. The reporting
/// surfaces call this, because they are the ones that would otherwise print a
/// zero: the observation has to be written down at the moment staleness
/// begins, or its age can never be known afterwards.
pub fn observe_index_in(
    state_dir: &std::path::Path,
    index_fingerprint: Option<&str>,
    tree_fingerprint: &str,
) -> IndexLiveness {
    observe_index_at(state_dir, index_fingerprint, tree_fingerprint, now())
}

/// `now` is a parameter so staleness ageing is testable.
pub fn observe_index_at(
    state_dir: &std::path::Path,
    index_fingerprint: Option<&str>,
    tree_fingerprint: &str,
    now: u64,
) -> IndexLiveness {
    let mut hb = load_from(state_dir);
    let previous_onset = hb.index.as_ref().and_then(|o| o.stale_since);
    let (verdict, stale_since) = match index_fingerprint {
        None => (IndexLiveness::NeverScanned, None),
        Some(fp) if fp == tree_fingerprint => (IndexLiveness::Current, None),
        Some(_) => {
            let since = previous_onset.unwrap_or(now);
            (IndexLiveness::Stale { stale_for: now.saturating_sub(since) }, Some(since))
        }
    };
    hb.index = Some(IndexObservation {
        index_fingerprint: index_fingerprint.map(str::to_string),
        tree_fingerprint: tree_fingerprint.to_string(),
        stale_since,
        observed_at: now,
    });
    store(state_dir, &hb);
    verdict
}

/// Compact age for report lines: an operator reads "9d", never 777600.
pub fn human_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
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

    #[test]
    fn a_registered_hook_with_no_record_is_unobserved_not_healthy() {
        // The defect: health was read off a failure counter alone, so a hook
        // that had never fired carried the same zero as one running cleanly.
        let dir = tempfile::tempdir().unwrap();
        record_ok_in(dir.path(), "session-start");
        let l = liveness_in(dir.path());
        assert_eq!(l.registered_count(), REGISTERED_HOOKS.len());
        assert!(matches!(
            l.hooks.iter().find(|h| h.name == "session-start").unwrap().state,
            HookState::Healthy { .. }
        ));
        assert_eq!(l.unobserved().len(), REGISTERED_HOOKS.len() - 1);
        assert!(l.unobserved().contains(&"stop"));
        assert!(l.degraded().is_empty(), "silence is not failure either");
        assert_eq!(l.severity(), Severity::Unobserved);
        let summary = l.summary();
        assert!(summary.contains("NEVER OBSERVED"), "{summary}");
        assert!(summary.contains("stop"), "name what is silent: {summary}");
        assert!(!summary.contains("all healthy"), "the forbidden sentence: {summary}");
    }

    #[test]
    fn an_empty_heartbeat_reports_blindness_rather_than_health() {
        let dir = tempfile::tempdir().unwrap();
        let l = liveness_in(dir.path());
        assert!(l.blind());
        assert_eq!(l.severity(), Severity::Unobserved);
        let summary = l.summary();
        assert!(summary.contains("unknown, not healthy"), "{summary}");
        assert!(!summary.contains("all healthy"), "{summary}");
    }

    #[test]
    fn every_registered_hook_reporting_is_the_only_healthy_state() {
        let dir = tempfile::tempdir().unwrap();
        for hook in REGISTERED_HOOKS {
            record_ok_in(dir.path(), hook);
        }
        let l = liveness_in(dir.path());
        assert_eq!(l.severity(), Severity::Healthy);
        assert!(l.unobserved().is_empty());
        assert!(
            l.summary().contains("all 6 registered hook(s) reporting, healthy"),
            "{}",
            l.summary()
        );

        // One failing hook outranks the rest being fine.
        for _ in 0..DEGRADED_AFTER {
            record_error_in(dir.path(), "stop", "boom");
        }
        let l = liveness_in(dir.path());
        assert_eq!(l.severity(), Severity::Failing);
        assert!(l.summary().contains("FAILING: stop (3×)"), "{}", l.summary());
    }

    #[test]
    fn a_record_from_a_hook_we_no_longer_register_is_labeled_not_counted() {
        let dir = tempfile::tempdir().unwrap();
        for hook in REGISTERED_HOOKS {
            record_ok_in(dir.path(), hook);
        }
        record_ok_in(dir.path(), "notification");
        let l = liveness_in(dir.path());
        assert_eq!(
            l.registered_count(),
            REGISTERED_HOOKS.len(),
            "an extra record is not an extra expectation"
        );
        assert!(l.summary().contains("unregistered record(s): notification"), "{}", l.summary());
    }

    #[test]
    fn an_index_that_was_never_scanned_is_not_a_current_one() {
        let dir = tempfile::tempdir().unwrap();
        let v = observe_index_at(dir.path(), None, "tree-fp", 1_000);
        assert_eq!(v, IndexLiveness::NeverScanned);
        assert_eq!(v.severity(), Severity::Unobserved);
        assert!(v.describe().contains("NEVER SCANNED"), "{}", v.describe());
        assert!(v.describe().contains("not zeros"), "{}", v.describe());
    }

    #[test]
    fn a_stale_index_ages_from_the_moment_it_first_disagreed() {
        // Staleness with no recorded onset can only ever say "stale"; the
        // operator needs "stale for nine days", and that is unrecoverable
        // after the fact because the catalog carries no build timestamp.
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(observe_index_at(dir.path(), Some("a"), "a", 1_000), IndexLiveness::Current);
        assert_eq!(
            observe_index_at(dir.path(), Some("a"), "b", 2_000),
            IndexLiveness::Stale { stale_for: 0 },
            "the first disagreement starts the clock"
        );
        let later = observe_index_at(dir.path(), Some("a"), "c", 2_000 + 9 * 86_400);
        assert_eq!(later, IndexLiveness::Stale { stale_for: 9 * 86_400 });
        assert_eq!(later.severity(), Severity::Failing);
        assert!(later.describe().contains("STALE for 9d"), "{}", later.describe());

        // A scan closes it, and the next lag starts a fresh clock.
        assert_eq!(observe_index_at(dir.path(), Some("c"), "c", 3_000_000), IndexLiveness::Current);
        assert!(load_from(dir.path()).index.unwrap().stale_since.is_none());
        assert_eq!(
            observe_index_at(dir.path(), Some("c"), "d", 3_000_060),
            IndexLiveness::Stale { stale_for: 0 }
        );
    }

    #[test]
    fn human_age_reads_as_an_operator_would_say_it() {
        assert_eq!(human_age(0), "0s");
        assert_eq!(human_age(90), "1m");
        assert_eq!(human_age(7_200), "2h");
        assert_eq!(human_age(9 * 86_400), "9d");
    }
}
