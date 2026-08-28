//! Change-driven vector upkeep owned by the daemon.
//!
//! The Markdown watcher advances the catalog generation. This worker reacts
//! to that generation, hydrates shared/peer artifacts first, and derives only
//! content hashes that remain missing. The clock only retries failures; it
//! never causes repeated inference against an unchanged generation.

use std::time::{Duration, Instant};

use crate::config::Config;
use crate::{embed, grant, index, paths, runtime_status, vectors};

const GENERATION_POLL: Duration = Duration::from_secs(2);
const SETTLE_DELAY: Duration = Duration::from_secs(2);
const FAILURE_RETRY: Duration = Duration::from_secs(5 * 60);
const BATCH: usize = 64;

fn generation() -> Option<u64> {
    index::open_ro(&paths::state_dir())
        .ok()
        .map(|connection| index::generation(&connection))
        .filter(|generation| *generation > 0)
}

/// A storage host has useful autonomous vector work when it may derive new
/// vectors, can request them from a joined peer, or already shares compatible
/// artifacts that this host's disposable cache may need to hydrate. A local
/// embedding endpoint is deliberately not required for the latter two paths.
pub(crate) fn sources_available(cfg: &Config, state_dir: &std::path::Path) -> bool {
    if cfg.client.serving.is_some() {
        return false;
    }
    cfg.embeddings.enabled
        || grant::memberships(state_dir).is_ok_and(|memberships| !memberships.is_empty())
        || vectors::VectorStore::open(&cfg.brain_root, &cfg.embeddings.spec())
            .is_ok_and(|store| !store.is_empty())
}

pub fn run(cfg: Config, stopping: impl Fn() -> bool) {
    if cfg.client.serving.is_some()
        || crate::embedding_profile::production_availability().is_err()
    {
        return;
    }
    let mut completed_generation = None;
    let mut due: Option<(u64, Instant)> = None;

    while !stopping() {
        // A peer may be joined, or a compatible shared artifact may appear,
        // after daemon startup. Keep the cheap worker resident so those
        // events do not require a restart; without a source it performs no
        // index write or inference call.
        if !sources_available(&cfg, &paths::state_dir()) {
            due = None;
            std::thread::sleep(GENERATION_POLL);
            continue;
        }
        if let Some(current) = generation()
            && completed_generation != Some(current)
            && due.map(|(scheduled, _)| scheduled) != Some(current)
        {
            due = Some((current, Instant::now() + SETTLE_DELAY));
        }

        if let Some((attempted_generation, deadline)) = due
            && Instant::now() >= deadline
        {
            match embed::sync_configured(&cfg, BATCH) {
                Ok((report, _)) => {
                    eprintln!(
                        "cfetch vectors: generation {attempted_generation}, {} embedded, {} imported, {} block(s) covered",
                        report.embedded,
                        report.imported,
                        report.total_blocks,
                    );
                    completed_generation = Some(attempted_generation);
                    due = None;
                    let _ = runtime_status::refresh_static();
                }
                Err(error) => {
                    eprintln!(
                        "cfetch vectors degraded at generation {attempted_generation}: {error:#}"
                    );
                    due = Some((attempted_generation, Instant::now() + FAILURE_RETRY));
                    let _ = runtime_status::refresh_static();
                }
            }
        }
        std::thread::sleep(GENERATION_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegated_worker_returns_without_waiting() {
        let started = Instant::now();
        let mut cfg = Config::default();
        cfg.client.serving = Some(crate::config::ClientServingConfig {
            addr: "127.0.0.1:1".into(),
            token_file: std::path::PathBuf::from("unused"),
        });
        run(cfg, || false);
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn inactive_profile_worker_returns_without_retrying() {
        let started = Instant::now();
        run(Config::default(), || false);
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn compatible_shared_artifacts_enable_hydration_without_an_endpoint() {
        let brain = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut cfg = Config {
            brain_root: brain.path().to_path_buf(),
            ..Config::default()
        };
        assert!(!sources_available(&cfg, state.path()));

        let spec = cfg.embeddings.spec();
        let mut store = vectors::VectorStore::open(&cfg.brain_root, &spec).unwrap();
        let mut writer = store.begin_write().unwrap();
        writer.put("content-hash", &vec![1.0; spec.dim]).unwrap();
        writer.flush().unwrap();
        drop(writer);
        assert!(sources_available(&cfg, state.path()));

        cfg.client.serving = Some(crate::config::ClientServingConfig {
            addr: "127.0.0.1:1".into(),
            token_file: std::path::PathBuf::from("unused"),
        });
        assert!(!sources_available(&cfg, state.path()));
    }
}
