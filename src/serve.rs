//! Serving mode: the daemon owns the index lifecycle for its brain tree.
//!
//! A recursive fs watcher (notify/inotify) turns every relevant event batch
//! into an index update; each committed update advances the monotonic
//! GENERATION persisted in the index meta. Every query op passes a DRAIN
//! BARRIER first — serve-fresh-or-wait, bounded: on timeout the answer still
//! comes, labeled `fresh: false` with a staleness note, never silently stale
//! and never hanging.
//!
//! Barrier mechanics: the caller drops a uniquely-numbered SENTINEL file into
//! a watched directory. The sentinel rides the same inotify queue as the real
//! events, so once the watcher has observed sentinel N, every write that
//! completed before the barrier began has been counted into `pending`; the
//! barrier then waits until the rebuild worker has applied that count. The
//! watcher is a latency optimization only — a stat-fingerprint check at
//! daemon start and every 60s is the correctness backstop for events missed
//! while the daemon was not running (or on filesystems inotify cannot see).
//!
//! This module also carries the CLIENT side: a none-tier host routes
//! recall/find/expand to a serving host over TCP (bearer-token gated, same
//! line-JSON protocol) and opens NO local index at all. Unreachable serving
//! host = explicit error naming the host — never a fallback to local data.
//!
//! Perf note: an update tries `index::rescan_changed` first — a stat-diff
//! that re-reads ONLY the changed files — and falls back to the full
//! `index::scan` for large diffs, a changed basis (different brain root,
//! never scanned), or any incremental failure. Both paths commit the same
//! catalog bytes and advance the generation identically; the barrier/
//! generation contract is unchanged.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use notify::Watcher as _;
use serde::{Deserialize, Serialize};

use crate::config::{ClientServingConfig, Config};
use crate::{daemon, index, paths};

/// Bounded wait for the drain barrier; on expiry the query answers anyway,
/// labeled stale.
pub const BARRIER_TIMEOUT: Duration = Duration::from_secs(5);
/// Event batches settle for this long before a rebuild picks them up.
const DEBOUNCE: Duration = Duration::from_millis(50);
/// Cadence of the stat-fingerprint correctness backstop.
const FINGERPRINT_INTERVAL: Duration = Duration::from_secs(60);
/// Remote client connect budget.
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
/// Remote client full-query budget.
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(10);

// ---- wire types (shared by server responses and remote clients) ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireHit {
    pub cite: String,
    pub path: String,
    pub ring: u8,
    pub start_line: usize,
    pub end_line: usize,
    pub snippet: String,
    #[serde(default)]
    pub mirrors: Vec<String>,
}

impl From<index::Hit> for WireHit {
    fn from(h: index::Hit) -> Self {
        WireHit {
            cite: h.cite,
            path: h.path,
            ring: h.ring,
            start_line: h.start_line,
            end_line: h.end_line,
            snippet: h.snippet,
            mirrors: h.mirrors,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireBlock {
    pub cite: String,
    pub path: String,
    pub ring: u8,
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
}

impl From<index::Block> for WireBlock {
    fn from(b: index::Block) -> Self {
        WireBlock {
            cite: b.cite,
            path: b.path,
            ring: b.ring,
            start_line: b.start_line,
            end_line: b.end_line,
            text: b.text,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireFindHit {
    pub path: String,
    pub name: Option<String>,
    pub kind: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub token_estimate: u64,
}

impl From<crate::code::FindHit> for WireFindHit {
    fn from(h: crate::code::FindHit) -> Self {
        WireFindHit {
            path: h.path,
            name: h.name,
            kind: h.kind,
            start_line: h.start_line,
            end_line: h.end_line,
            token_estimate: h.token_estimate,
        }
    }
}

// ---- server state ----

#[derive(Debug, Default)]
struct Progress {
    /// Real fs event batches observed by the watcher.
    pending: u64,
    /// `pending` value covered by the last committed index update.
    applied: u64,
    /// Highest barrier sentinel sequence the watcher has observed.
    sentinel_seen: u64,
    /// The startup backstop (or first rebuild) has completed — before this,
    /// nothing may claim freshness: events may have been missed while down.
    settled: bool,
    /// Tree watch registration has finished. Freshness additionally requires
    /// this: between listener bind and full registration, a write in a
    /// not-yet-watched directory would be invisible to the barrier.
    watches_ready: bool,
    /// Last index-update failure; cleared by the next success.
    last_error: Option<String>,
}

pub struct ServeState {
    pub origin: String,
    state_dir: PathBuf,
    barrier_dir: PathBuf,
    progress: Mutex<Progress>,
    cv: Condvar,
    barrier_seq: AtomicU64,
    pub generation: AtomicU64,
    pub last_barrier_ms: AtomicU64,
    /// Actual TCP listen address once bound (resolves ":0" configs).
    pub bind_addr: Mutex<Option<String>>,
}

pub struct BarrierOutcome {
    pub fresh: bool,
    pub waited_ms: u64,
    pub note: Option<String>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ServeState {
    pub(crate) fn new(origin: String, state_dir: PathBuf, barrier_dir: PathBuf) -> Self {
        ServeState {
            origin,
            state_dir,
            barrier_dir,
            progress: Mutex::new(Progress::default()),
            cv: Condvar::new(),
            barrier_seq: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            last_barrier_ms: AtomicU64::new(0),
            bind_addr: Mutex::new(None),
        }
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// The drain barrier: serve-fresh-or-wait, bounded. See the module doc
    /// for the ordering argument.
    pub fn barrier(&self, timeout: Duration) -> BarrierOutcome {
        let start = Instant::now();
        let deadline = start + timeout;
        let seq = self.barrier_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let sentinel = self.barrier_dir.join(format!("b{seq}"));
        let mut fresh = true;
        let mut note = None;
        if std::fs::write(&sentinel, seq.to_string()).is_ok() {
            if !self.wait_until(deadline, |p| p.sentinel_seen >= seq) {
                fresh = false;
                note = Some(
                    "barrier timeout: the fs watcher has not observed the barrier sentinel"
                        .to_string(),
                );
            }
        } else {
            fresh = false;
            note = Some("barrier unavailable: could not write the barrier sentinel".to_string());
        }
        if fresh {
            let target = lock(&self.progress).pending;
            if !self.wait_until(deadline, |p| p.settled && p.watches_ready && p.applied >= target) {
                fresh = false;
                let err = lock(&self.progress).last_error.clone();
                note = Some(match err {
                    Some(e) => format!("barrier timeout: pending changes not applied (last index error: {e})"),
                    None => "barrier timeout: pending changes not yet applied".to_string(),
                });
            }
        }
        let _ = std::fs::remove_file(&sentinel);
        let waited_ms = start.elapsed().as_millis() as u64;
        self.last_barrier_ms.store(waited_ms, Ordering::Relaxed);
        BarrierOutcome { fresh, waited_ms, note }
    }

    fn wait_until(&self, deadline: Instant, done: impl Fn(&Progress) -> bool) -> bool {
        let mut guard = lock(&self.progress);
        loop {
            if done(&guard) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            guard = self
                .cv
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Watcher glue: a real (non-sentinel) event batch was observed.
    pub(crate) fn note_event(&self) {
        let mut p = lock(&self.progress);
        p.pending += 1;
        drop(p);
        self.cv.notify_all();
    }

    /// Watcher glue: barrier sentinel `seq` was observed.
    pub(crate) fn note_sentinel(&self, seq: u64) {
        let mut p = lock(&self.progress);
        if seq > p.sentinel_seen {
            p.sentinel_seen = seq;
        }
        drop(p);
        self.cv.notify_all();
    }

    /// Worker glue: an index update covering `target` committed at
    /// `generation`.
    pub(crate) fn mark_applied(&self, target: u64, generation: u64) {
        self.generation.store(generation, Ordering::Relaxed);
        let mut p = lock(&self.progress);
        if target > p.applied {
            p.applied = target;
        }
        p.settled = true;
        p.last_error = None;
        drop(p);
        self.cv.notify_all();
    }

    /// Worker glue: an index update failed. `applied` does not advance and
    /// `settled` is not set — barriers will (correctly) label answers stale.
    pub(crate) fn mark_error(&self, err: String) {
        let mut p = lock(&self.progress);
        p.last_error = Some(err);
        drop(p);
        self.cv.notify_all();
    }

    fn pending_now(&self) -> u64 {
        lock(&self.progress).pending
    }

    fn is_settled(&self) -> bool {
        let p = lock(&self.progress);
        p.settled && p.watches_ready
    }

    /// Registration thread glue: every tree watch is in place.
    pub(crate) fn mark_watches_ready(&self) {
        lock(&self.progress).watches_ready = true;
        self.cv.notify_all();
    }

    pub(crate) fn watches_ready(&self) -> bool {
        lock(&self.progress).watches_ready
    }

    fn applied_now(&self) -> u64 {
        lock(&self.progress).applied
    }
}

/// Serving-host identity: explicit `serve.origin`, else the machine hostname.
pub fn origin_of(cfg: &Config) -> String {
    if let Some(o) = &cfg.serve.origin
        && !o.is_empty()
    {
        return o.clone();
    }
    hostname()
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "unknown-host".to_string())
}

/// Keeps the watcher alive for the daemon's lifetime.
pub struct ServeHandle {
    pub state: Arc<ServeState>,
    _watcher: Arc<Mutex<notify::RecommendedWatcher>>,
}

/// Directories the INDEXER would walk — the exact set worth watching.
///
/// Three defects made a recursive watch on the tree root undeployable on a
/// real brain, all found by deploying it (2026-08-21):
///   1. it followed symlinks — a wineprefix `dosdevices/z: -> /` inside a
///      checkout sent the watch walking the entire root filesystem;
///   2. it watched subtrees the indexer excludes (`projects/`, gitignored
///      scratch dumps), needing ~524k watches against a 524,288 default;
///   3. registration ran before the listener bound, so a restart was a
///      multi-minute serving outage.
///
/// Reusing the indexer's own walker settles 1 and 2 by construction: the same
/// `ignore` walker, the same gitignore/hidden rules, `follow_links(false)`,
/// and the same exclusion predicate — so the watch set is exactly the index
/// set, never a byte more. 3 is settled by registering in the background
/// (see `start`), with `settled` withheld until registration completes so no
/// answer claims freshness it cannot have.
fn watchable_dirs(brain_root: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![brain_root.to_path_buf()];
    let walker = ignore::WalkBuilder::new(brain_root)
        .hidden(true)
        .git_ignore(true)
        .follow_links(false)
        .build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(brain_root) else { continue };
        let rel = rel.to_string_lossy();
        if rel.is_empty() || crate::index::excluded_dir(&rel) {
            continue;
        }
        dirs.push(entry.path().to_path_buf());
    }
    dirs
}

/// Registers one non-recursive watch per indexable directory. Non-recursive
/// is what keeps a symlinked directory from dragging its target in: notify
/// only ever sees the paths we hand it.
fn register_watches(
    watcher: &Arc<Mutex<notify::RecommendedWatcher>>,
    dirs: &[PathBuf],
) -> (usize, usize) {
    let mut ok = 0usize;
    let mut failed = 0usize;
    for dir in dirs {
        let res = lock(watcher).watch(dir, notify::RecursiveMode::NonRecursive);
        match res {
            Ok(()) => ok += 1,
            // A directory that vanished between walk and watch, or a watch
            // limit hit: the 60s fingerprint backstop still covers it.
            Err(_) => failed += 1,
        }
    }
    (ok, failed)
}

#[cfg(test)]
mod watch_scope_tests {
    use super::*;

    /// The three deployment defects, as tests: never follow a symlink out of
    /// the tree, never watch what the indexer excludes, and watch only
    /// directories that exist.
    #[test]
    fn watchable_dirs_skips_symlinks_and_excluded_subtrees() {
        let brain = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(outside.path().join("deep/deeper")).unwrap();
        for d in ["knowledge/hosts", "mind/memories", "mind/secrets", "logs/x", "projects/repo/src", "knowledge/archive/old"] {
            std::fs::create_dir_all(brain.path().join(d)).unwrap();
        }
        // The wineprefix-style escape hatch that walked the whole rootfs.
        std::os::unix::fs::symlink(outside.path(), brain.path().join("knowledge/z")).unwrap();

        let dirs = watchable_dirs(brain.path());
        let rel: Vec<String> = dirs
            .iter()
            .map(|d| d.strip_prefix(brain.path()).unwrap().to_string_lossy().to_string())
            .collect();

        assert!(rel.iter().any(|r| r == "knowledge/hosts"));
        assert!(rel.iter().any(|r| r == "mind/memories"));
        assert!(!rel.iter().any(|r| r.starts_with("mind/secrets")), "secrets never watched: {rel:?}");
        assert!(!rel.iter().any(|r| r.starts_with("logs")), "logs excluded: {rel:?}");
        assert!(!rel.iter().any(|r| r.starts_with("projects")), "projects excluded: {rel:?}");
        assert!(!rel.iter().any(|r| r.starts_with("knowledge/archive")), "archive excluded: {rel:?}");
        assert!(
            !dirs.iter().any(|d| d.starts_with(outside.path())),
            "a symlinked directory must never drag its target in: {dirs:?}"
        );
    }

    #[test]
    fn every_watchable_dir_is_a_real_directory() {
        let brain = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(brain.path().join("knowledge")).unwrap();
        std::fs::write(brain.path().join("knowledge/a.md"), "x\n").unwrap();
        for d in watchable_dirs(brain.path()) {
            assert!(d.is_dir(), "{d:?} is not a directory");
        }
    }

    #[test]
    fn freshness_requires_watch_registration() {
        // A state whose backstop settled but whose watches are not yet
        // registered must NOT be treated as settled: a write in an
        // unregistered directory would be invisible to the barrier.
        let dir = tempfile::tempdir().unwrap();
        let state = ServeState::new(
            "o".to_string(),
            dir.path().to_path_buf(),
            dir.path().join("barrier"),
        );
        state.mark_applied(0, 1);
        assert!(!state.is_settled(), "settled must require watches_ready");
        state.mark_watches_ready();
        assert!(state.is_settled());
    }
}

/// Parses `<barrier_dir>/b<seq>`; None for anything else (a real tree path).
fn sentinel_seq_of(barrier_dir: &Path, path: &Path) -> Option<u64> {
    let rel = path.strip_prefix(barrier_dir).ok()?;
    rel.to_str()?.strip_prefix('b')?.parse().ok()
}

fn on_event(state: &ServeState, wake: &mpsc::Sender<()>, event: &notify::Event) {
    // Access events fire for every file the indexer itself reads; counting
    // them would make each rebuild trigger the next.
    if matches!(event.kind, notify::EventKind::Access(_)) {
        return;
    }
    let mut sentinel_max = 0u64;
    let mut real = false;
    for path in &event.paths {
        match sentinel_seq_of(&state.barrier_dir, path) {
            Some(seq) => sentinel_max = sentinel_max.max(seq),
            None => real = true,
        }
    }
    if real {
        state.note_event();
        let _ = wake.send(());
    }
    if sentinel_max > 0 {
        state.note_sentinel(sentinel_max);
    }
}

/// Starts watcher + rebuild worker. The returned handle must live as long as
/// serving does.
pub fn start(cfg: &Config) -> anyhow::Result<ServeHandle> {
    let state_dir = paths::state_dir();
    let barrier_dir = state_dir.join("barrier");
    std::fs::create_dir_all(&barrier_dir)?;
    // Leftover sentinels from a previous run would satisfy this run's low
    // sequence numbers; clear them.
    if let Ok(rd) = std::fs::read_dir(&barrier_dir) {
        for e in rd.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let state = Arc::new(ServeState::new(origin_of(cfg), state_dir, barrier_dir.clone()));
    let (wake_tx, wake_rx) = mpsc::channel::<()>();
    let watcher = notify::recommended_watcher({
        let state = state.clone();
        let wake_tx = wake_tx.clone();
        move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                on_event(&state, &wake_tx, &ev);
            }
        }
    })?;
    let watcher = Arc::new(Mutex::new(watcher));
    // The barrier directory is tiny and always watchable: register it inline
    // so a barrier taken during startup still resolves.
    lock(&watcher).watch(&barrier_dir, notify::RecursiveMode::NonRecursive)?;

    // Tree watches are registered in the BACKGROUND: enumerating a large
    // brain takes seconds to minutes, and the caller (daemon::run) must reach
    // its listener bind immediately — a restart may never be a serving
    // outage. Until this completes the worker withholds `settled`, so answers
    // are labeled stale rather than wrongly fresh.
    std::thread::spawn({
        let watcher = watcher.clone();
        let state = state.clone();
        let brain_root = cfg.brain_root.clone();
        let wake_tx = wake_tx.clone();
        move || {
            let dirs = watchable_dirs(&brain_root);
            let (ok, failed) = register_watches(&watcher, &dirs);
            if failed > 0 {
                eprintln!(
                    "cfetch serve: watching {ok} director(ies); {failed} could not be watched \
                     (the 60s fingerprint backstop still covers them)"
                );
            }
            state.mark_watches_ready();
            // A directory may have appeared while we were registering.
            let _ = wake_tx.send(());
        }
    });

    std::thread::spawn({
        let state = state.clone();
        let cfg = cfg.clone();
        let watcher = watcher.clone();
        move || worker(&state, &cfg, &wake_rx, &watcher)
    });
    Ok(ServeHandle { state, _watcher: watcher })
}

/// The rebuild worker: drains event batches into index updates and runs the
/// fingerprint backstop at start and every 60s.
fn worker(
    state: &ServeState,
    cfg: &Config,
    wake: &mpsc::Receiver<()>,
    watcher: &Arc<Mutex<notify::RecommendedWatcher>>,
) {
    let native = paths::native_projects_root();
    let mut conn: Option<rusqlite::Connection> = None;
    // Startup backstop: whatever happened while the daemon was down is
    // invisible to the watcher.
    apply(state, &mut conn, cfg, &native, true);
    let mut last_backstop = Instant::now();
    let mut watched: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    loop {
        match wake.recv_timeout(FINGERPRINT_INTERVAL) {
            Ok(()) => {
                std::thread::sleep(DEBOUNCE);
                while wake.try_recv().is_ok() {}
                apply(state, &mut conn, cfg, &native, false);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        if last_backstop.elapsed() >= FINGERPRINT_INTERVAL {
            last_backstop = Instant::now();
            apply(state, &mut conn, cfg, &native, true);
            // Directories created since the last sweep carry no watch of
            // their own (watches are per-directory, non-recursive by design).
            // Re-enumerating on the backstop cadence keeps the watch set
            // convergent without ever following a symlink.
            if state.watches_ready() {
                let dirs = watchable_dirs(&cfg.brain_root);
                if watched.is_empty() {
                    watched = dirs.iter().cloned().collect();
                } else {
                    let fresh: Vec<PathBuf> =
                        dirs.iter().filter(|d| !watched.contains(*d)).cloned().collect();
                    if !fresh.is_empty() {
                        register_watches(watcher, &fresh);
                        watched.extend(fresh);
                    }
                }
            }
        }
    }
}

/// One update pass. `backstop` additionally runs the stat-fingerprint check
/// (correctness floor); the event path trusts the watcher's dirty marker.
fn apply(
    state: &ServeState,
    conn: &mut Option<rusqlite::Connection>,
    cfg: &Config,
    native: &Path,
    backstop: bool,
) {
    if conn.is_none() {
        match index::open(&state.state_dir) {
            Ok(c) => {
                // The rebuild may queue behind the daemon's background code
                // scan; give the writer more patience than a default reader.
                let _ = c.busy_timeout(Duration::from_secs(10));
                // The read-only query connections cannot create the code
                // tables lazily; make sure they exist before serving `find`.
                if let Err(e) = crate::code::ensure_schema(&c) {
                    state.mark_error(format!("code schema: {e}"));
                    return;
                }
                *conn = Some(c);
            }
            Err(e) => {
                state.mark_error(format!("open index: {e}"));
                return;
            }
        }
    }
    let Some(c) = conn.as_mut() else { return };
    let target = state.pending_now();
    let dirty = target > state.applied_now();
    let need_scan = if backstop {
        dirty || index::stale(c, &cfg.brain_root, Some(native)).unwrap_or(true)
    } else {
        dirty
    };
    if need_scan {
        // Incremental first: re-scan only the changed files. `None` (diff too
        // large, basis changed) and errors alike fall back to the full scan —
        // the correctness floor either way.
        let result = match index::rescan_changed(c, &cfg.brain_root, Some(native)) {
            Ok(Some(r)) => Ok(r),
            Ok(None) | Err(_) => index::scan(c, &cfg.brain_root, Some(native)),
        };
        match result {
            Ok(r) => state.mark_applied(target, r.generation),
            Err(e) => state.mark_error(e.to_string()),
        }
    } else if !state.is_settled() {
        // Fingerprint says the committed catalog already describes the tree.
        state.mark_applied(target, index::generation(c));
    }
}

// ---- token handling ----

/// Reads a bearer token file, trimmed. `require_0600` additionally refuses
/// group/other-accessible files — the serving daemon must not accept a
/// world-readable credential as its gate.
pub fn read_token(path: &Path, require_0600: bool) -> anyhow::Result<String> {
    if require_0600 {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(path)
                .map_err(|e| anyhow::anyhow!("token file {}: {e}", path.display()))?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                anyhow::bail!(
                    "token file {} must be 0600 (mode is {:o})",
                    path.display(),
                    mode & 0o777
                );
            }
        }
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("token file {}: {e}", path.display()))?;
    let token = raw.trim().to_string();
    if token.is_empty() {
        anyhow::bail!("token file {} is empty", path.display());
    }
    Ok(token)
}

/// Constant-time-ish comparison: never early-returns on the first differing
/// byte, so response timing does not leak the token prefix.
pub fn token_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
    }
    diff == 0
}

// ---- client side (none-tier host) ----

/// One request/response against a serving host's TCP listener. Every failure
/// names the serving host — a none-tier host has no local data to fall back
/// to, and must never pretend otherwise.
pub fn remote_request(
    addr: &str,
    token: &str,
    mut body: serde_json::Value,
    read_timeout: Duration,
) -> anyhow::Result<daemon::Response> {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::net::{TcpStream, ToSocketAddrs as _};
    let sock_addr = addr
        .to_socket_addrs()
        .map_err(|e| anyhow::anyhow!("serving host {addr}: bad address: {e}"))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("serving host {addr}: address resolves to nothing"))?;
    let stream = TcpStream::connect_timeout(&sock_addr, CONNECT_TIMEOUT)
        .map_err(|e| anyhow::anyhow!("serving host {addr} unreachable: {e}"))?;
    stream.set_read_timeout(Some(read_timeout))?;
    stream.set_write_timeout(Some(read_timeout))?;
    body["token"] = serde_json::Value::String(token.to_string());
    let mut stream = stream;
    writeln!(stream, "{body}").map_err(|e| anyhow::anyhow!("serving host {addr}: send: {e}"))?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|e| anyhow::anyhow!("serving host {addr}: read: {e}"))?;
    let resp: daemon::Response = serde_json::from_str(&line)
        .map_err(|e| anyhow::anyhow!("serving host {addr}: bad response: {e}"))?;
    if !resp.ok {
        anyhow::bail!(
            "serving host {addr} refused: {}",
            resp.error.unwrap_or_else(|| "unknown error".to_string())
        );
    }
    Ok(resp)
}

/// Client call using `client.serving` config (token loaded from its file).
pub fn client_call(
    cs: &ClientServingConfig,
    body: serde_json::Value,
    read_timeout: Duration,
) -> anyhow::Result<daemon::Response> {
    let token = read_token(&cs.token_file, false)
        .map_err(|e| anyhow::anyhow!("client.serving token: {e}"))?;
    remote_request(&cs.addr, &token, body, read_timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_eq_matches_only_exact() {
        assert!(token_eq("abc123", "abc123"));
        assert!(!token_eq("abc123", "abc124"));
        assert!(!token_eq("abc123", "abc12"));
        assert!(!token_eq("", "x"));
        assert!(token_eq("", ""));
    }

    #[test]
    fn read_token_enforces_0600_and_trims() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("token");
        std::fs::write(&p, "  secret-token\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_token(&p, true).is_err(), "0644 must be refused for the server gate");
        assert_eq!(read_token(&p, false).unwrap(), "secret-token");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_token(&p, true).unwrap(), "secret-token");
        std::fs::write(&p, "\n").unwrap();
        assert!(read_token(&p, false).is_err(), "empty token file is an error");
    }

    #[test]
    fn barrier_times_out_stale_when_nothing_applies() {
        let dir = tempfile::tempdir().unwrap();
        let state = ServeState::new(
            "test".into(),
            dir.path().to_path_buf(),
            dir.path().join("barrier"),
        );
        std::fs::create_dir_all(dir.path().join("barrier")).unwrap();
        // No watcher, no worker: the sentinel is never observed.
        let out = state.barrier(Duration::from_millis(80));
        assert!(!out.fresh, "an unserviced barrier must label the answer stale");
        assert!(out.note.is_some());
        assert!(out.waited_ms >= 80);
    }

    #[test]
    fn barrier_waits_for_sentinel_then_applied() {
        let dir = tempfile::tempdir().unwrap();
        let barrier_dir = dir.path().join("barrier");
        std::fs::create_dir_all(&barrier_dir).unwrap();
        let state = Arc::new(ServeState::new(
            "test".into(),
            dir.path().to_path_buf(),
            barrier_dir,
        ));
        // A fully started server: watches registered (the registration
        // thread's job), pending work exists and is not yet applied.
        state.mark_watches_ready();
        state.note_event();
        // A fake watcher+worker: observes the sentinel, then applies.
        let sim = std::thread::spawn({
            let state = state.clone();
            move || {
                std::thread::sleep(Duration::from_millis(30));
                state.note_sentinel(1);
                std::thread::sleep(Duration::from_millis(30));
                state.mark_applied(1, 7);
            }
        });
        let out = state.barrier(Duration::from_secs(2));
        sim.join().unwrap();
        assert!(out.fresh, "note: {:?}", out.note);
        assert!(out.waited_ms >= 60, "must have waited for sentinel AND apply");
        assert_eq!(state.generation.load(Ordering::Relaxed), 7);
    }

    #[test]
    fn barrier_is_stale_until_startup_settles() {
        let dir = tempfile::tempdir().unwrap();
        let barrier_dir = dir.path().join("barrier");
        std::fs::create_dir_all(&barrier_dir).unwrap();
        let state = Arc::new(ServeState::new(
            "test".into(),
            dir.path().to_path_buf(),
            barrier_dir,
        ));
        // Sentinel observed immediately, zero pending — but the startup
        // backstop has not settled yet: events may have been missed while
        // the daemon was down, so freshness must not be claimed.
        let sim = std::thread::spawn({
            let state = state.clone();
            move || {
                std::thread::sleep(Duration::from_millis(10));
                state.note_sentinel(1);
            }
        });
        let out = state.barrier(Duration::from_millis(100));
        sim.join().unwrap();
        assert!(!out.fresh, "unsettled startup must not serve fresh");
        // Applying is not enough while tree watches are still registering:
        // a write in an unwatched directory would be invisible.
        state.mark_applied(0, 1);
        std::thread::spawn({
            let state = state.clone();
            move || {
                std::thread::sleep(Duration::from_millis(10));
                state.note_sentinel(2);
            }
        })
        .join()
        .unwrap();
        let out = state.barrier(Duration::from_millis(100));
        assert!(!out.fresh, "unregistered watches must not serve fresh");
        state.mark_watches_ready();
        std::thread::spawn({
            let state = state.clone();
            move || {
                std::thread::sleep(Duration::from_millis(10));
                state.note_sentinel(3);
            }
        })
        .join()
        .unwrap();
        let out = state.barrier(Duration::from_secs(1));
        assert!(out.fresh);
    }

    #[test]
    fn failed_update_makes_barrier_stale_with_the_error_named() {
        let dir = tempfile::tempdir().unwrap();
        let barrier_dir = dir.path().join("barrier");
        std::fs::create_dir_all(&barrier_dir).unwrap();
        let state = Arc::new(ServeState::new(
            "test".into(),
            dir.path().to_path_buf(),
            barrier_dir,
        ));
        state.mark_applied(0, 1); // settled
        state.note_event();
        state.mark_error("disk on fire".to_string());
        let sim = std::thread::spawn({
            let state = state.clone();
            move || {
                std::thread::sleep(Duration::from_millis(10));
                state.note_sentinel(1);
            }
        });
        let out = state.barrier(Duration::from_millis(120));
        sim.join().unwrap();
        assert!(!out.fresh);
        assert!(out.note.as_deref().unwrap().contains("disk on fire"));
    }

    #[test]
    fn sentinel_paths_are_distinguished_from_tree_paths() {
        let bd = PathBuf::from("/state/barrier");
        assert_eq!(sentinel_seq_of(&bd, Path::new("/state/barrier/b17")), Some(17));
        assert_eq!(sentinel_seq_of(&bd, Path::new("/state/barrier/bx")), None);
        assert_eq!(sentinel_seq_of(&bd, Path::new("/brain/knowledge/b17")), None);
        assert_eq!(sentinel_seq_of(&bd, Path::new("/state/barrier")), None);
    }

    #[test]
    fn origin_prefers_config_over_hostname() {
        let mut cfg = Config::default();
        cfg.serve.origin = Some("storage-1".to_string());
        assert_eq!(origin_of(&cfg), "storage-1");
        cfg.serve.origin = None;
        assert!(!origin_of(&cfg).is_empty(), "hostname fallback must yield something");
    }
}
