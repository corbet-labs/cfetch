//! Change-driven background maintenance owned by the warm daemon.
//!
//! The worker polls only a cheap content revision. A model call is caused by a
//! changed candidate set, never by the clock alone; the retry timer merely
//! keeps a transient endpoint failure from leaving durable evidence stranded.

use std::time::{Duration, Instant};

use crate::config::Config;
use crate::{maintenance, maintenance_model};

const REVISION_POLL: Duration = Duration::from_secs(2);
const FAILURE_RETRY: Duration = Duration::from_secs(5 * 60);

pub fn run(cfg: Config, stopping: impl Fn() -> bool) {
    if !cfg.maintenance.enabled || !cfg.maintenance.configured() {
        return;
    }
    let debounce = Duration::from_secs(cfg.maintenance.debounce_secs.max(1));
    let mut observed_revision: Option<String> = None;
    let mut due: Option<Instant> = None;

    while !stopping() {
        let revision = maintenance::candidate_revision(&cfg);
        let candidates = crate::staging::pending_count(&crate::paths::staging_dir(&cfg.brain_root));
        if maintenance::is_paused(&cfg) || candidates == 0 {
            due = None;
        } else if observed_revision.as_deref() != Some(revision.as_str()) {
            // Every new edit resets the quiet period, so the packet sees a
            // settled batch rather than racing a burst of hook writes.
            observed_revision = Some(revision);
            due = Some(Instant::now() + debounce);
        }

        if due.is_some_and(|deadline| Instant::now() >= deadline) {
            match maintenance_model::MaintenanceClient::new(&cfg.maintenance).and_then(|mut model| {
                maintenance::run_once_with(&cfg, &mut model, cfg.maintenance.max_candidates)
            }) {
                Ok(report) => {
                    eprintln!(
                        "cfetch maintenance: {} examined, {} applied, {} dismissed, {} noop, {} exception(s)",
                        report.examined,
                        report.applied,
                        report.dismissed,
                        report.noops,
                        report.exceptions
                    );
                    let remaining = crate::staging::pending_count(&crate::paths::staging_dir(&cfg.brain_root));
                    observed_revision = Some(maintenance::candidate_revision(&cfg));
                    due = (remaining > 0).then(|| Instant::now() + FAILURE_RETRY);
                }
                Err(error) => {
                    eprintln!("cfetch maintenance degraded: {error:#}");
                    due = Some(Instant::now() + FAILURE_RETRY);
                }
            }
        }
        std::thread::sleep(REVISION_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unconfigured_or_disabled_worker_returns_without_waiting() {
        let cfg = Config::default();
        let started = Instant::now();
        run(cfg, || false);
        assert!(started.elapsed() < Duration::from_millis(100));

        let mut cfg = Config::default();
        cfg.maintenance.endpoint = "http://127.0.0.1:1/v1".into();
        cfg.maintenance.model = "model".into();
        cfg.maintenance.enabled = false;
        run(cfg, || false);
        assert!(started.elapsed() < Duration::from_millis(100));
    }
}
