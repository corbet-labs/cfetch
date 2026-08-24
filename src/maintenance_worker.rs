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
    let mut journaled_preflight_failure: Option<String> = None;

    while !stopping() {
        let revision = maintenance::candidate_revision(&cfg);
        let candidates = crate::staging::pending_count(&crate::paths::staging_dir(&cfg.brain_root));
        if maintenance::is_paused(&cfg) {
            // Forget the pre-pause revision. Resuming must schedule the
            // evidence that was already observed instead of waiting for a
            // second edit to wake the worker.
            observed_revision = None;
            due = None;
        } else if candidates == 0 {
            due = None;
        } else if observed_revision.as_deref() != Some(revision.as_str()) {
            // Every new edit resets the quiet period, so the packet sees a
            // settled batch rather than racing a burst of hook writes.
            observed_revision = Some(revision);
            due = Some(Instant::now() + debounce);
            journaled_preflight_failure = None;
        }

        if due.is_some_and(|deadline| Instant::now() >= deadline) {
            let cycle = match maintenance_model::MaintenanceClient::new(&cfg.maintenance) {
                Ok(mut model) => {
                    maintenance::run_once_with(&cfg, &mut model, cfg.maintenance.max_candidates)
                }
                Err(error) => {
                    crate::runtime_status::record_maintenance_attempt(
                        crate::runtime_status::endpoint_route(&cfg.maintenance.endpoint),
                        "proposal",
                        false,
                    );
                    if journaled_preflight_failure.as_deref() != Some(revision.as_str()) {
                        let _ = maintenance::record_background_exception(&cfg, error.to_string());
                        journaled_preflight_failure = Some(revision.clone());
                    }
                    Err(error)
                }
            };
            match cycle {
                Ok(report) => {
                    journaled_preflight_failure = None;
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
                    due = (remaining > 0).then(|| {
                        Instant::now()
                            + if report.exceptions > 0 {
                                FAILURE_RETRY
                            } else {
                                debounce
                            }
                    });
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
