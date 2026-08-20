//! The warm per-host daemon. Hooks are thin clients: they must never open
//! databases or scan trees themselves (they sit on the interactive path), so
//! anything heavier than a file read lives behind this Unix socket.
//!
//! Protocol: one JSON object per line in, one per line out, then the server
//! closes the connection. Milestone 1 ops: ping, resident, health, shutdown.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

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

fn handle(req: &Request, cfg: &Config) -> (Response, bool) {
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
            let d = resident::build(cfg);
            (Response { ok: true, digest: Some(d.text), ..Response::default() }, false)
        }
        "health" => {
            let degraded = heartbeat::degraded().into_iter().map(|(n, _)| n).collect();
            (Response { ok: true, degraded_hooks: Some(degraded), ..Response::default() }, false)
        }
        "shutdown" => (Response { ok: true, ..Response::default() }, true),
        other => (
            Response { ok: false, error: Some(format!("unknown op: {other}")), ..Response::default() },
            false,
        ),
    }
}

/// Foreground run loop (systemd/`daemon start` both end up here).
pub fn run() -> anyhow::Result<()> {
    let cfg = Config::load()?;
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
            Ok(req) => handle(&req, &cfg),
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
