//! The warm per-host daemon. Hooks are thin clients: they must never open
//! databases or scan trees themselves (they sit on the interactive path), so
//! anything heavier than a file read lives behind this Unix socket.
//!
//! Protocol: one JSON object per line in, one per line out, then the server
//! closes the connection. Ops: ping, resident, health, scan-code,
//! scan-status, shutdown.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::{heartbeat, paths, resident};

#[derive(Debug, Deserialize)]
struct Request {
    op: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degraded_hooks: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scan: Option<ScanStatus>,
}

/// Counts from the most recent SUCCESSFUL code scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanCounts {
    pub files: usize,
    pub symbols: usize,
    pub edges: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanStatus {
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_finished: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_counts: Option<ScanCounts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Single-flight gate for the background code scan. A scan over NFS code
/// roots takes minutes, so the daemon runs it on a detached thread; a second
/// request while one runs is REFUSED, not queued — the second scan would only
/// re-read the same tree.
pub struct ScanCoordinator {
    inner: Mutex<ScanStatus>,
}

impl ScanCoordinator {
    pub const fn new() -> Self {
        ScanCoordinator {
            inner: Mutex::new(ScanStatus {
                running: false,
                last_finished: None,
                last_counts: None,
                last_error: None,
            }),
        }
    }

    /// A panicked scan thread must not wedge status reporting.
    fn lock(&self) -> std::sync::MutexGuard<'_, ScanStatus> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Claims the single scan slot; false while another scan runs.
    pub fn try_begin(&self) -> bool {
        let mut s = self.lock();
        if s.running {
            return false;
        }
        s.running = true;
        true
    }

    /// Releases the slot. An error records `last_error` without erasing the
    /// last good counts; a success clears the error.
    pub fn complete(&self, result: Result<ScanCounts, String>) {
        let mut s = self.lock();
        s.running = false;
        s.last_finished = Some(now_secs());
        match result {
            Ok(c) => {
                s.last_counts = Some(c);
                s.last_error = None;
            }
            Err(e) => s.last_error = Some(e),
        }
    }

    pub fn status(&self) -> ScanStatus {
        self.lock().clone()
    }
}

impl Default for ScanCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

static SCAN: ScanCoordinator = ScanCoordinator::new();

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn run_code_scan() -> Result<ScanCounts, String> {
    let cfg = Config::load().map_err(|e| e.to_string())?;
    let mut conn = crate::index::open(&paths::state_dir()).map_err(|e| e.to_string())?;
    let r = crate::code::scan_code(&mut conn, &cfg.effective_code_roots()).map_err(|e| e.to_string())?;
    Ok(ScanCounts { files: r.files, symbols: r.symbols, edges: r.edges })
}

/// Client call with a hard deadline. The hook path budget is ~250ms total; on
/// any failure the caller falls back or stays silent.
pub fn call(op: &str, timeout: Duration) -> Option<Response> {
    let stream = UnixStream::connect(paths::socket_path()).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    let mut stream = stream;
    let req = serde_json::json!({ "op": op });
    writeln!(stream, "{req}").ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

fn handle(req: &Request) -> (Response, bool) {
    match req.op.as_str() {
        "ping" => (
            Response {
                ok: true,
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
                ..Response::default()
            },
            false,
        ),
        "resident" => {
            // Config is reloaded per request: a startup snapshot would make
            // the warm path silently diverge from the daemon-less fallback
            // after a config edit.
            match Config::load() {
                Ok(cfg) => {
                    let d = resident::build(&cfg);
                    (Response { ok: true, digest: Some(d.text), ..Response::default() }, false)
                }
                Err(e) => (
                    Response { ok: false, error: Some(e.to_string()), ..Response::default() },
                    false,
                ),
            }
        }
        "health" => {
            let degraded = heartbeat::degraded().into_iter().map(|(n, _)| n).collect();
            (Response { ok: true, degraded_hooks: Some(degraded), ..Response::default() }, false)
        }
        "scan-code" => {
            if SCAN.try_begin() {
                // Detached worker: the daemon keeps answering while the scan
                // (minutes on NFS) runs; a panic still releases the slot.
                std::thread::spawn(|| {
                    let result = std::panic::catch_unwind(run_code_scan)
                        .unwrap_or_else(|_| Err("code scan thread panicked".to_string()));
                    SCAN.complete(result);
                });
                (Response { ok: true, scan: Some(SCAN.status()), ..Response::default() }, false)
            } else {
                (
                    Response {
                        ok: false,
                        error: Some("a code scan is already running".to_string()),
                        scan: Some(SCAN.status()),
                        ..Response::default()
                    },
                    false,
                )
            }
        }
        "scan-status" => (Response { ok: true, scan: Some(SCAN.status()), ..Response::default() }, false),
        "shutdown" => (Response { ok: true, ..Response::default() }, true),
        other => (
            Response { ok: false, error: Some(format!("unknown op: {other}")), ..Response::default() },
            false,
        ),
    }
}

/// Foreground run loop (systemd/`daemon start` both end up here).
pub fn run() -> anyhow::Result<()> {
    let sock = paths::socket_path();
    if let Some(dir) = sock.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // A stale socket file from a dead daemon must be cleared; a live one must
    // not be stolen.
    if sock.exists() {
        if UnixStream::connect(&sock).is_ok() {
            anyhow::bail!("daemon already running on {}", sock.display());
        }
        std::fs::remove_file(&sock)?;
    }
    let listener = UnixListener::bind(&sock)?;
    eprintln!("cfetch daemon listening on {}", sock.display());
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
        let mut reader = BufReader::new(conn);
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            continue;
        }
        let (resp, shutdown) = match serde_json::from_str::<Request>(&line) {
            Ok(req) => handle(&req),
            Err(e) => (
                Response { ok: false, error: Some(format!("bad request: {e}")), ..Response::default() },
                false,
            ),
        };
        let mut conn = reader.into_inner();
        if let Ok(s) = serde_json::to_string(&resp) {
            let _ = writeln!(conn, "{s}");
        }
        if shutdown {
            break;
        }
    }
    let _ = std::fs::remove_file(&sock);
    Ok(())
}

/// Detached start: re-executes this binary with `daemon run`.
pub fn start() -> anyhow::Result<()> {
    if call("ping", Duration::from_millis(300)).is_some() {
        println!("daemon already running");
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .args(["daemon", "run"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(50));
        if call("ping", Duration::from_millis(200)).is_some() {
            println!("daemon started on {}", paths::socket_path().display());
            return Ok(());
        }
    }
    anyhow::bail!("daemon did not answer after start")
}

pub fn stop() -> anyhow::Result<()> {
    match call("shutdown", Duration::from_millis(500)) {
        Some(r) if r.ok => {
            println!("daemon stopped");
            Ok(())
        }
        _ => {
            println!("daemon not running");
            Ok(())
        }
    }
}

pub fn status() -> anyhow::Result<()> {
    match call("ping", Duration::from_millis(300)) {
        Some(r) => {
            println!("daemon: running (v{})", r.version.unwrap_or_default());
            if let Some(s) = call("scan-status", Duration::from_millis(300)).and_then(|r| r.scan) {
                if s.running {
                    println!("code scan: running in the background");
                } else if let Some(t) = s.last_finished {
                    let ago = now_secs().saturating_sub(t);
                    match &s.last_counts {
                        Some(c) => println!(
                            "code scan: finished {ago}s ago ({} files, {} symbols, {} import edges)",
                            c.files, c.symbols, c.edges
                        ),
                        None => println!("code scan: finished {ago}s ago"),
                    }
                }
                if let Some(e) = &s.last_error {
                    println!("code scan: last error: {e}");
                }
            }
        }
        None => println!("daemon: not running ({})", paths::socket_path().display()),
    }
    let degraded = heartbeat::degraded();
    if degraded.is_empty() {
        println!("hooks: healthy");
    } else {
        for (name, h) in degraded {
            println!(
                "hooks: {} failing ({} consecutive; last: {})",
                name,
                h.consecutive_failures,
                h.last_error.unwrap_or_default()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_coordinator_is_single_flight() {
        let c = ScanCoordinator::new();
        assert!(c.try_begin());
        assert!(!c.try_begin(), "a second scan must be refused while one runs");
        assert!(c.status().running);
        c.complete(Ok(ScanCounts { files: 3, symbols: 5, edges: 2 }));
        let s = c.status();
        assert!(!s.running);
        assert!(s.last_finished.is_some());
        assert_eq!(s.last_counts.as_ref().map(|c| c.files), Some(3));
        assert!(c.try_begin(), "a finished scan frees the slot");
    }

    #[test]
    fn scan_coordinator_error_keeps_last_good_counts() {
        let c = ScanCoordinator::new();
        assert!(c.try_begin());
        c.complete(Ok(ScanCounts { files: 7, symbols: 9, edges: 1 }));
        assert!(c.try_begin());
        c.complete(Err("boom".to_string()));
        let s = c.status();
        assert!(!s.running);
        assert_eq!(s.last_error.as_deref(), Some("boom"));
        assert_eq!(s.last_counts.as_ref().map(|c| c.files), Some(7), "an error must not erase the last good counts");
        assert!(c.try_begin());
        c.complete(Ok(ScanCounts { files: 8, symbols: 9, edges: 1 }));
        assert!(c.status().last_error.is_none(), "success clears the error");
    }
}
