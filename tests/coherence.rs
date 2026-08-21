//! Coherence torture harness for serving mode.
//!
//! Proves the PRD's guarantees against a REAL daemon process (the compiled
//! binary, spawned per test against temp trees):
//!   (a) read-your-writes through the drain barrier, N concurrent writers,
//!       zero tolerance;
//!   (b) monotonic prefix across concurrent writers;
//!   (c) catalog determinism: fresh scan vs event-driven incremental builds
//!       yield EQUAL checksums;
//!   (d) crash-restart: the stat-fingerprint backstop catches up on writes
//!       made while the daemon was dead;
//!   (e) the same guarantees over the serving TCP listener (bearer-token
//!       gated) as over the LOCAL control channel.
//!
//! The local channel is the platform's: a unix socket on unix, token-gated
//! loopback TCP on Windows (see `src/ipc.rs`). [`Local`] is the one place
//! that difference exists in this harness — every test below speaks to it
//! identically.
//!
//! The same harness runs against a LIVE deployment: set CFETCH_TORTURE_ADDR
//! and CFETCH_TORTURE_TOKEN (optionally CFETCH_TORTURE_QUERY) and the
//! read-only live test exercises generation monotonicity, per-generation
//! checksum stability and freshness labeling on the real serving host.

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpStream;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_cfetch");

struct Daemon {
    child: Child,
    state: PathBuf,
    // Kept alive for the daemon's lifetime.
    _home: tempfile::TempDir,
}

/// A handle on one daemon's LOCAL control channel. Cloneable and `Send` so
/// the concurrency tests can hand it to their writer threads, exactly as they
/// handed a socket path before.
#[cfg(unix)]
#[derive(Clone)]
struct Local(PathBuf);

#[cfg(windows)]
#[derive(Clone)]
struct Local {
    addr: String,
    token: String,
}

impl Local {
    /// Reads the endpoint a daemon publishes into its state dir. `None` until
    /// it has been published.
    #[cfg(unix)]
    fn published(state: &Path) -> Option<Local> {
        let p = state.join("daemon.sock");
        if p.exists() { Some(Local(p)) } else { None }
    }

    #[cfg(windows)]
    fn published(state: &Path) -> Option<Local> {
        let raw = std::fs::read_to_string(state.join("daemon.endpoint")).ok()?;
        let mut lines = raw.lines();
        let addr = lines.next()?.trim().to_string();
        let token = lines.next()?.trim().to_string();
        if addr.is_empty() || token.is_empty() {
            return None;
        }
        Some(Local { addr, token })
    }

    #[cfg(unix)]
    fn describe(&self) -> String {
        self.0.display().to_string()
    }

    #[cfg(windows)]
    fn describe(&self) -> String {
        format!("tcp {}", self.addr)
    }

    /// One request over the local channel; `None` when it does not answer.
    #[cfg(unix)]
    fn req_opt(&self, body: &Value) -> Option<Value> {
        let mut s = UnixStream::connect(&self.0).ok()?;
        s.set_read_timeout(Some(Duration::from_secs(15))).ok()?;
        s.set_write_timeout(Some(Duration::from_secs(15))).ok()?;
        writeln!(s, "{body}").ok()?;
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).ok()?;
        serde_json::from_str(&line).ok()
    }

    #[cfg(windows)]
    fn req_opt(&self, body: &Value) -> Option<Value> {
        let mut body = body.clone();
        body["token"] = Value::String(self.token.clone());
        let mut s = TcpStream::connect(&self.addr).ok()?;
        s.set_read_timeout(Some(Duration::from_secs(15))).ok()?;
        s.set_write_timeout(Some(Duration::from_secs(15))).ok()?;
        writeln!(s, "{body}").ok()?;
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).ok()?;
        serde_json::from_str(&line).ok()
    }

    fn req(&self, body: &Value) -> Value {
        self.req_opt(body)
            .unwrap_or_else(|| panic!("daemon did not answer on {}", self.describe()))
    }
}

impl Daemon {
    /// The daemon's local control channel.
    fn local(&self) -> Local {
        Local::published(&self.state).expect("daemon published its local endpoint")
    }

    /// Actual TCP address once bound (written by the daemon; resolves ":0").
    fn tcp_addr(&self) -> String {
        let p = self.state.join("serve.addr");
        for _ in 0..100 {
            if let Ok(s) = std::fs::read_to_string(&p)
                && !s.trim().is_empty()
            {
                return s.trim().to_string();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("daemon never wrote {}", p.display());
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Spawns `cfetch daemon run` against its own state dir + config, waits until
/// the local control channel answers ping.
fn start_daemon(brain: &Path, state: &Path, serve_extra: Value) -> Daemon {
    std::fs::create_dir_all(state).unwrap();
    let home = tempfile::tempdir().unwrap();
    let cfg_path = state.join("config.json");
    let mut serve = json!({"enabled": true});
    if let Some(map) = serve_extra.as_object() {
        for (k, v) in map {
            serve[k] = v.clone();
        }
    }
    let cfg = json!({
        "brain_root": brain.to_string_lossy(),
        "resident": [],
        "capture": {"enabled": false},
        "serve": serve,
    });
    std::fs::write(&cfg_path, serde_json::to_string(&cfg).unwrap()).unwrap();
    let child = Command::new(BIN)
        .args(["daemon", "run"])
        .env("CFETCH_STATE_DIR", state)
        .env("CFETCH_CONFIG", &cfg_path)
        .env("HOME", home.path())
        .env_remove("XDG_RUNTIME_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon");
    let d = Daemon { child, state: state.to_path_buf(), _home: home };
    for _ in 0..200 {
        if Local::published(&d.state)
            .and_then(|l| l.req_opt(&json!({"op": "ping"})))
            .is_some_and(|r| r["ok"] == true)
        {
            return d;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("daemon did not become ready in {}", d.state.display());
}

fn tcp_req(addr: &str, token: &str, body: &Value) -> Value {
    let mut body = body.clone();
    body["token"] = Value::String(token.to_string());
    let mut s = TcpStream::connect(addr).expect("tcp connect");
    s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    s.set_write_timeout(Some(Duration::from_secs(15))).unwrap();
    writeln!(s, "{body}").unwrap();
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).expect("tcp read");
    serde_json::from_str(&line).expect("tcp response parses")
}

/// All hit snippets of a recall response, concatenated for containment checks.
fn snippet_blob(resp: &Value) -> String {
    resp["hits"]
        .as_array()
        .map(|hits| {
            hits.iter()
                .filter_map(|h| h["snippet"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn append_line(path: &Path, line: &str) {
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new().append(true).create(true).open(path).unwrap();
    // One write syscall per line: a concurrent scan sees whole lines only.
    f.write_all(line.as_bytes()).unwrap();
}

fn write_token_file(dir: &Path, token: &str) -> PathBuf {
    let p = dir.join("token");
    std::fs::write(&p, format!("{token}\n")).unwrap();
    // The serving daemon refuses a group/other-readable token file. Windows
    // has no mode bits and `serve::read_token` documents that gap.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    p
}

// ---- (a) read-your-writes + observed-committed prefix, concurrent ----

#[test]
fn read_your_writes_under_concurrent_writers() {
    const WRITERS: usize = 4;
    const ITERS: usize = 75; // 300 barrier round-trips total

    let brain = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(brain.path().join("knowledge")).unwrap();
    for w in 0..WRITERS {
        std::fs::write(brain.path().join(format!("knowledge/w{w}.md")), "").unwrap();
    }
    let state = tempfile::tempdir().unwrap();
    let daemon = start_daemon(brain.path(), state.path(), json!({"origin": "torture-origin"}));
    let local = daemon.local();

    // Every (writer, seq) whose append has COMPLETED. A query snapshotting
    // this set must see every member — that is the whole guarantee.
    let committed: Arc<Mutex<Vec<(usize, usize)>>> = Arc::new(Mutex::new(Vec::new()));
    let handles: Vec<_> = (0..WRITERS)
        .map(|w| {
            let committed = committed.clone();
            let local = local.clone();
            let file = brain.path().join(format!("knowledge/w{w}.md"));
            std::thread::spawn(move || {
                for n in 1..=ITERS {
                    append_line(&file, &format!("- torture writer{w} seq{n} tk{w}x{n}\n"));
                    committed.lock().unwrap().push((w, n));
                    let snapshot = committed.lock().unwrap().clone();
                    let resp =
                        local.req(&json!({"op": "recall", "query": "torture", "limit": 100000}));
                    assert_eq!(resp["ok"], true, "query failed: {resp}");
                    assert_eq!(
                        resp["fresh"], true,
                        "barrier must serve fresh under this load (writer {w} seq {n}): {resp}"
                    );
                    assert_eq!(resp["origin"], "torture-origin");
                    let blob = snippet_blob(&resp);
                    for (cw, cn) in &snapshot {
                        assert!(
                            blob.contains(&format!("tk{cw}x{cn}")),
                            "writer {w} seq {n}: committed statement tk{cw}x{cn} missing from a \
                             fresh-labeled answer (zero tolerance)"
                        );
                    }
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

// ---- (b) monotonic prefix seen by a concurrent reader ----

#[test]
fn monotonic_prefix_across_concurrent_writers() {
    const WRITERS: usize = 3;
    const ITERS: usize = 50;
    const READS: usize = 40;

    let brain = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(brain.path().join("knowledge")).unwrap();
    for w in 0..WRITERS {
        std::fs::write(brain.path().join(format!("knowledge/w{w}.md")), "").unwrap();
    }
    let state = tempfile::tempdir().unwrap();
    let daemon = start_daemon(brain.path(), state.path(), json!({}));
    let local = daemon.local();

    let writers: Vec<_> = (0..WRITERS)
        .map(|w| {
            let file = brain.path().join(format!("knowledge/w{w}.md"));
            std::thread::spawn(move || {
                for n in 1..=ITERS {
                    append_line(&file, &format!("- torture writer{w} seq{n} tk{w}x{n}\n"));
                    std::thread::sleep(Duration::from_millis(2));
                }
            })
        })
        .collect();

    for _ in 0..READS {
        let resp = local.req(&json!({"op": "recall", "query": "torture", "limit": 100000}));
        assert_eq!(resp["ok"], true, "{resp}");
        assert_eq!(resp["fresh"], true, "{resp}");
        let blob = snippet_blob(&resp);
        for w in 0..WRITERS {
            let max_seen = (1..=ITERS)
                .filter(|n| blob.contains(&format!("tk{w}x{n}")))
                .max()
                .unwrap_or(0);
            for n in 1..=max_seen {
                assert!(
                    blob.contains(&format!("tk{w}x{n}")),
                    "gap in writer {w}'s prefix: seq {n} missing while seq {max_seen} visible"
                );
            }
        }
    }
    for h in writers {
        h.join().unwrap();
    }
}

// ---- (c) determinism: fresh scan vs incremental-via-events ----

#[test]
fn checksum_deterministic_fresh_vs_incremental() {
    let brain = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(brain.path().join("knowledge")).unwrap();
    std::fs::write(brain.path().join("knowledge/base.md"), "- base fact\n").unwrap();

    // Daemon A builds INCREMENTALLY: initial scan, then five event batches.
    let state_a = tempfile::tempdir().unwrap();
    let daemon_a = start_daemon(brain.path(), state_a.path(), json!({}));
    for i in 0..5 {
        std::fs::write(
            brain.path().join(format!("knowledge/inc{i}.md")),
            format!("- incremental fact {i}\n\nparagraph {i}\n"),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    append_line(&brain.path().join("knowledge/base.md"), "- appended after start\n");
    let a = daemon_a.local().req(&json!({"op": "checksum"}));
    assert_eq!(a["ok"], true, "{a}");
    assert_eq!(a["fresh"], true, "{a}");
    let checksum_a = a["checksum"].as_str().unwrap().to_string();
    assert!(!checksum_a.is_empty());

    // Daemon B derives FRESH from the finished tree in its own state dir.
    let state_b = tempfile::tempdir().unwrap();
    let daemon_b = start_daemon(brain.path(), state_b.path(), json!({}));
    let b = daemon_b.local().req(&json!({"op": "checksum"}));
    assert_eq!(b["ok"], true, "{b}");
    assert_eq!(
        b["checksum"].as_str().unwrap(),
        checksum_a,
        "incremental and fresh catalog derivations must agree byte-for-byte"
    );
    // Generations are per-holder histories and may differ; the CATALOG agrees.
    assert!(a["generation"].as_u64().unwrap() >= 1);
    assert!(b["generation"].as_u64().unwrap() >= 1);
}

// ---- (d) crash + restart: the fingerprint backstop catches up ----

#[test]
fn crash_restart_backstop_catches_up() {
    let brain = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(brain.path().join("knowledge")).unwrap();
    std::fs::write(brain.path().join("knowledge/a.md"), "- fact one\n").unwrap();

    let state = tempfile::tempdir().unwrap();
    let mut daemon = start_daemon(brain.path(), state.path(), json!({}));
    let before = daemon.local().req(&json!({"op": "checksum"}));
    assert_eq!(before["ok"], true);
    let checksum_before = before["checksum"].as_str().unwrap().to_string();

    // SIGKILL mid-life; write while nothing is watching.
    daemon.kill();
    append_line(&brain.path().join("knowledge/a.md"), "- fact two, written while daemon dead\n");
    std::fs::write(brain.path().join("knowledge/new.md"), "- born during the outage\n").unwrap();

    // Restart on the SAME state dir: the startup fingerprint backstop must
    // reconcile before the first barrier releases.
    let daemon2 = start_daemon(brain.path(), state.path(), json!({}));
    let after = daemon2.local().req(&json!({"op": "checksum"}));
    assert_eq!(after["ok"], true, "{after}");
    assert_eq!(after["fresh"], true, "{after}");
    let checksum_after = after["checksum"].as_str().unwrap().to_string();
    assert_ne!(checksum_after, checksum_before, "outage writes must change the catalog");

    // Ground truth: a fresh derivation over the final tree.
    let state_c = tempfile::tempdir().unwrap();
    let daemon_c = start_daemon(brain.path(), state_c.path(), json!({}));
    let fresh = daemon_c.local().req(&json!({"op": "checksum"}));
    assert_eq!(fresh["checksum"].as_str().unwrap(), checksum_after);

    // And recall actually surfaces the outage write.
    let resp = daemon2.local().req(&json!({"op": "recall", "query": "outage", "limit": 10}));
    assert_eq!(resp["ok"], true);
    assert!(snippet_blob(&resp).contains("born during the outage"));
}

// ---- (e) remote client over TCP: same guarantees, token-gated ----

#[test]
fn tcp_client_gets_the_same_guarantees() {
    let brain = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(brain.path().join("knowledge")).unwrap();
    std::fs::write(brain.path().join("knowledge/a.md"), "- seed fact\n").unwrap();

    let state = tempfile::tempdir().unwrap();
    let token_dir = tempfile::tempdir().unwrap();
    let token = "torture-bearer-token";
    let token_file = write_token_file(token_dir.path(), token);
    let daemon = start_daemon(
        brain.path(),
        state.path(),
        json!({
            "bind": "127.0.0.1:0",
            "origin": "tcp-origin",
            "token_file": token_file.to_string_lossy(),
        }),
    );
    let addr = daemon.tcp_addr();

    // Wrong/missing token: refused, no data.
    let denied = tcp_req(&addr, "wrong-token", &json!({"op": "recall", "query": "seed"}));
    assert_eq!(denied["ok"], false);
    assert_eq!(denied["error"], "unauthorized");
    assert!(denied.get("hits").is_none());

    // Read-your-writes over TCP, exactly like the unix path.
    for n in 1..=20 {
        append_line(
            &brain.path().join("knowledge/a.md"),
            &format!("- torture remote seq{n} rtk{n}\n"),
        );
        let resp = tcp_req(&addr, token, &json!({"op": "recall", "query": "torture", "limit": 1000}));
        assert_eq!(resp["ok"], true, "{resp}");
        assert_eq!(resp["fresh"], true, "{resp}");
        assert_eq!(resp["origin"], "tcp-origin");
        let blob = snippet_blob(&resp);
        for m in 1..=n {
            assert!(blob.contains(&format!("rtk{m}")), "remote seq {m} missing at iteration {n}");
        }
    }

    // Generation + checksum ops over TCP; the serving listener and the local
    // control channel agree on the catalog.
    let g = tcp_req(&addr, token, &json!({"op": "generation"}));
    assert_eq!(g["ok"], true);
    assert!(g["generation"].as_u64().unwrap() >= 1);
    let tcp_sum = tcp_req(&addr, token, &json!({"op": "checksum"}));
    let local_sum = daemon.local().req(&json!({"op": "checksum"}));
    assert_eq!(tcp_sum["checksum"], local_sum["checksum"]);

    // find + slices answer over TCP too (empty here — no code scan ran —
    // but shaped and labeled).
    let f = tcp_req(&addr, token, &json!({"op": "find", "query": "anything"}));
    assert_eq!(f["ok"], true, "{f}");
    assert!(f["fresh"].is_boolean() && f["origin"] == "tcp-origin");
    assert!(f["code_hits"].as_array().unwrap().is_empty());
    let s = tcp_req(&addr, token, &json!({"op": "slices", "path": "/x.rs", "limit": 3}));
    assert_eq!(s["ok"], true, "{s}");
    assert!(s["slices"].as_array().unwrap().is_empty());

    // Expand round-trip: cite from a TCP recall expands over TCP.
    let resp = tcp_req(&addr, token, &json!({"op": "recall", "query": "rtk7", "limit": 5}));
    let cite = resp["hits"][0]["cite"].as_str().unwrap().to_string();
    let expanded = tcp_req(&addr, token, &json!({"op": "expand", "cite": cite}));
    assert_eq!(expanded["ok"], true);
    assert!(expanded["blocks"][0]["text"].as_str().unwrap().contains("rtk7"));
}

// ---- none-tier CLI routing against a serving host ----

#[test]
fn none_tier_cli_routes_remotely_and_opens_no_local_index() {
    let brain = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(brain.path().join("knowledge")).unwrap();
    std::fs::write(brain.path().join("knowledge/a.md"), "- unique remotefact zylkor\n").unwrap();

    let state = tempfile::tempdir().unwrap();
    let token_dir = tempfile::tempdir().unwrap();
    let token_file = write_token_file(token_dir.path(), "cli-token");
    let daemon = start_daemon(
        brain.path(),
        state.path(),
        json!({
            "bind": "127.0.0.1:0",
            "origin": "storage-host",
            "token_file": token_file.to_string_lossy(),
        }),
    );
    let addr = daemon.tcp_addr();

    // The none-tier client: empty brain, no local index, remote routing.
    let client_home = tempfile::tempdir().unwrap();
    let client_state = tempfile::tempdir().unwrap();
    let empty_brain = tempfile::tempdir().unwrap();
    let client_cfg = client_state.path().join("config.json");
    std::fs::write(
        &client_cfg,
        serde_json::to_string(&json!({
            "brain_root": empty_brain.path().to_string_lossy(),
            "resident": [],
            "client": {"serving": {"addr": addr, "token_file": token_file.to_string_lossy()}},
        }))
        .unwrap(),
    )
    .unwrap();
    let run = |args: &[&str]| {
        Command::new(BIN)
            .args(args)
            .env("CFETCH_STATE_DIR", client_state.path())
            .env("CFETCH_CONFIG", &client_cfg)
            .env("HOME", client_home.path())
            .env_remove("XDG_RUNTIME_DIR")
            .output()
            .unwrap()
    };

    let out = run(&["recall", "zylkor"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "recall failed: {stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("zylkor"), "remote hit missing: {stdout}");
    assert!(stdout.contains("served by storage-host"), "coherence footer missing: {stdout}");
    assert!(stdout.contains("fresh"), "freshness label missing: {stdout}");
    assert!(
        !client_state.path().join("index.db").exists(),
        "none-tier host must open NO local index at all"
    );

    // A none-tier host refuses to build a parallel local truth.
    let out = run(&["scan"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("none-tier"));

    // Unreachable serving host: explicit error naming the host, nonzero exit,
    // never a silent local fallback.
    std::fs::write(
        &client_cfg,
        serde_json::to_string(&json!({
            "brain_root": empty_brain.path().to_string_lossy(),
            "resident": [],
            "client": {"serving": {"addr": "127.0.0.1:9", "token_file": token_file.to_string_lossy()}},
        }))
        .unwrap(),
    )
    .unwrap();
    let out = run(&["recall", "zylkor"]);
    assert!(!out.status.success(), "unreachable serving host must be a hard error");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("127.0.0.1:9"), "error must name the serving host: {stderr}");
}

// ---- live-fleet run (read-only), env-gated ----

#[test]
fn live_serving_host_coherence() {
    let Ok(addr) = std::env::var("CFETCH_TORTURE_ADDR") else {
        eprintln!("skipped: CFETCH_TORTURE_ADDR not set (live run only)");
        return;
    };
    let token = std::env::var("CFETCH_TORTURE_TOKEN")
        .expect("CFETCH_TORTURE_TOKEN must be set together with CFETCH_TORTURE_ADDR");

    let g1 = tcp_req(&addr, &token, &json!({"op": "generation"}));
    assert_eq!(g1["ok"], true, "{g1}");
    assert!(g1["fresh"].is_boolean(), "freshness must be labeled: {g1}");
    let gen1 = g1["generation"].as_u64().unwrap();

    let c1 = tcp_req(&addr, &token, &json!({"op": "checksum"}));
    let c2 = tcp_req(&addr, &token, &json!({"op": "checksum"}));
    assert_eq!(c1["ok"], true);
    assert_eq!(c2["ok"], true);
    if c1["generation"] == c2["generation"] {
        assert_eq!(
            c1["checksum"], c2["checksum"],
            "same generation must mean same catalog checksum"
        );
    }

    let g2 = tcp_req(&addr, &token, &json!({"op": "generation"}));
    assert!(
        g2["generation"].as_u64().unwrap() >= gen1,
        "generation must be monotonic: {gen1} then {g2}"
    );

    let query = std::env::var("CFETCH_TORTURE_QUERY").unwrap_or_else(|_| "readme".to_string());
    let r = tcp_req(&addr, &token, &json!({"op": "recall", "query": query, "limit": 5}));
    assert_eq!(r["ok"], true, "{r}");
    assert!(r["origin"].is_string() && r["fresh"].is_boolean(), "coherence labels missing: {r}");
}
