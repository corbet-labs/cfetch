//! The warm per-host daemon. Hooks are thin clients: they must never open
//! databases or scan trees themselves (they sit on the interactive path), so
//! anything heavier than a file read lives behind the LOCAL control channel
//! ([`crate::ipc`]: a unix socket on unix, loopback TCP on Windows).
//!
//! Protocol: one JSON object per line in, one per line out, then the server
//! closes the connection. Ops: ping, resident, health, scan-code,
//! scan-status, serve-status, shutdown — plus, when serving mode is enabled
//! (config `serve.enabled`), the barrier-gated query ops recall, expand,
//! find, map, slices, generation and checksum.
//!
//! A serving daemon also keeps its OWN code index current: once the tree
//! watches are registered it kicks the single-flight background code scan and
//! repeats it on the fingerprint cadence. Nothing outside has to send
//! `scan-code` for `find`/`map` to answer on a freshly started host.
//!
//! With `serve.bind` set, the SAME protocol is additionally served over TCP,
//! gated by a bearer token (`token` field in every request; token sourced
//! from `serve.token_file`, 0600). Shutdown is refused on that listener.
//!
//! Three channels, one gate. [`Channel`] is the whole policy surface: whether
//! a connection must present a token, and whether it may shut the daemon
//! down. A unix socket is access-controlled by its file mode and needs no
//! token; loopback TCP is not, so the Windows LOCAL channel presents the
//! daemon's own token — checked by the same `serve::token_eq` comparison the
//! serving listener uses, never a second implementation.

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::{heartbeat, hooks, index, ipc, paths, resident, serve};

#[derive(Debug, Default, Deserialize)]
struct Request {
    op: String,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    cite: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    /// map op: personalize the ranking toward this term.
    #[serde(default)]
    focus: Option<String>,
    /// map op: token budget the rendered map must fit.
    #[serde(default)]
    budget_tokens: Option<u64>,
    /// recall op: rank by vectors (`semantic`) or fuse with BM25 (`hybrid`).
    /// A none-tier host cannot embed its own query — the host holding the
    /// index does it, which is the whole point of serving.
    #[serde(default)]
    semantic: Option<bool>,
    #[serde(default)]
    hybrid: Option<bool>,
    /// SessionStart's working directory, for the resident op: the daemon
    /// shares the host with its caller but not the cwd, so the repo half of
    /// the injection scope has to travel with the request.
    #[serde(default)]
    cwd: Option<String>,
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
    // ---- serving-mode fields ----
    /// Host id of the answering serving host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Catalog generation the answer was served from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    /// True when the drain barrier fully applied before answering; false =
    /// bounded-barrier expiry, see `stale_note`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fresh: Option<bool>,
    /// Why the answer may be stale (only when `fresh` is false).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_note: Option<String>,
    /// Time this query spent in the drain barrier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub barrier_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    /// How the ranking degraded, if it did — absent vector coverage, an
    /// unreachable reranker. It travels with the answer because the client
    /// that asked cannot see the endpoint the serving host talked to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hits: Option<Vec<serve::WireHit>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocks: Option<Vec<serve::WireBlock>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_hits: Option<Vec<serve::WireFindHit>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub map: Option<serve::WireMap>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slices: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serve: Option<ServeInfo>,
}

impl Response {
    fn err(msg: impl Into<String>) -> Response {
        Response { ok: false, error: Some(msg.into()), ..Response::default() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServeInfo {
    pub enabled: bool,
    pub origin: String,
    pub generation: u64,
    pub last_barrier_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    /// Which drain-barrier coverage proof this host's watcher backend can
    /// support — `ordered (sentinel)` or `unordered (fingerprint)`. An
    /// operator must be able to see whether their platform is on the fast or
    /// the sound-and-slower path (see `serve::BarrierMode`). Defaulted for
    /// wire compatibility with a serving host older than the split.
    #[serde(default)]
    pub barrier_mode: String,
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

/// How often the scan cadence checks whether it may start.
const SCAN_READY_POLL: Duration = Duration::from_millis(200);
/// How long the first code scan waits for the markdown index to settle before
/// starting regardless. SQLite has ONE writer: a cold code scan holds the write
/// lock for as long as it walks, so letting the tree index (which the drain
/// barrier depends on) finish first keeps a fresh host's answers fresh. The
/// bound is there so a tree index that never settles cannot mean a code index
/// that never exists.
const FIRST_SCAN_SETTLE_WAIT: Duration = Duration::from_secs(300);

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn run_code_scan() -> Result<ScanCounts, String> {
    let cfg = Config::load().map_err(|e| e.to_string())?;
    let mut conn = crate::index::open(&paths::state_dir()).map_err(|e| e.to_string())?;
    // Background work with no interactive deadline: waiting out the tree
    // index's write transaction beats failing on a locked database.
    let _ = conn.busy_timeout(Duration::from_secs(30));
    let r = crate::code::scan_code(&mut conn, &cfg.effective_code_roots()).map_err(|e| e.to_string())?;
    Ok(ScanCounts { files: r.files, symbols: r.symbols, edges: r.edges })
}

/// Starts the background code scan unless one is already running. Returns
/// whether THIS call started it (single flight: a refusal is not an error,
/// the running scan re-reads the same tree anyway).
fn begin_code_scan() -> bool {
    if !SCAN.try_begin() {
        return false;
    }
    // Detached worker: the daemon keeps answering while the scan (minutes on
    // a cold network tree) runs; a panic still releases the slot.
    std::thread::spawn(|| {
        let result = std::panic::catch_unwind(run_code_scan)
            .unwrap_or_else(|_| Err("code scan thread panicked".to_string()));
        SCAN.complete(result);
    });
    true
}

/// The daemon's own code-scan cadence, on its own thread.
///
/// The deployed defect this fixes: a fresh serving host answered `find` and
/// `map` with "no hits" until somebody sent `scan-code` by hand. A serving
/// host owns its index lifecycle for the markdown tree already; the code index
/// is the same obligation.
///
/// Waits for the tree watches first (registering them and walking the code
/// roots at once would only contend for the same io), then kicks the
/// single-flight scan and repeats it on the fingerprint cadence — the
/// incremental scan is nearly free when nothing changed. Neither the listener
/// nor the drain barrier ever waits on this: the scan runs detached and the
/// barrier tracks the markdown watcher, not the code index.
fn scan_cadence(
    watches_ready: impl Fn() -> bool,
    kick: impl Fn() -> bool,
    poll: Duration,
    cadence: Duration,
    rounds: Option<usize>,
) {
    while !watches_ready() {
        std::thread::sleep(poll);
    }
    let mut done = 0usize;
    loop {
        kick();
        done += 1;
        if rounds.is_some_and(|r| done >= r) {
            return;
        }
        std::thread::sleep(cadence);
    }
}

/// Whether the first code scan may start: the tree watches are registered and
/// the tree index has settled — or the bounded wait for that has expired.
fn first_scan_ready(state: &serve::ServeState, deadline: Instant) -> bool {
    state.is_settled() || (state.watches_ready() && Instant::now() >= deadline)
}

/// The one line `cfetch status` leads with: which side of the serving topology
/// this host is on. Burying it is how a none-tier host reads as "empty" when
/// it is merely remote.
/// What a serving host reports as its barrier mode. A serving host older
/// than the mode split sends nothing; say so rather than imply the fast path.
fn barrier_mode_of(i: &ServeInfo) -> &str {
    if i.barrier_mode.is_empty() { "mode not reported" } else { &i.barrier_mode }
}

pub fn mode_line(cfg: &Config, info: Option<&ServeInfo>) -> String {
    if let Some(cs) = &cfg.client.serving {
        return format!(
            "mode: none-tier — no local index; recall/find/expand/map served by {}",
            cs.addr
        );
    }
    if cfg.serve.enabled {
        return match info.filter(|i| i.enabled) {
            Some(i) => format!(
                "mode: serving host {} (generation {}, {}, barrier {})",
                i.origin,
                i.generation,
                i.bind.clone().map_or_else(|| "unix socket only".to_string(), |b| format!("tcp {b}")),
                barrier_mode_of(i),
            ),
            None => format!(
                "mode: serving host {} (daemon down — generation unknown, nothing is being served)",
                serve::origin_of(cfg)
            ),
        };
    }
    "mode: local index only (not serving, no serving host configured)".to_string()
}

/// Client call with a hard deadline. The hook path budget is ~250ms total; on
/// any failure the caller falls back or stays silent.
pub fn call(op: &str, timeout: Duration) -> Option<Response> {
    call_req(&serde_json::json!({ "op": op }), timeout)
}

/// Structured client call over the local control channel.
pub fn call_req(body: &serde_json::Value, timeout: Duration) -> Option<Response> {
    let mut stream = ipc::connect(timeout)?;
    // A no-op where the transport is access-controlled by the operating
    // system: on unix the request goes out byte-for-byte as built here.
    let body = ipc::authenticate(body);
    writeln!(stream, "{body}").ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

/// Shared state of one daemon process across its connection threads.
struct Ctx {
    /// The daemon's own configuration. A serving host ranks ON BEHALF OF
    /// clients that hold nothing, so the endpoints it may reach — embeddings,
    /// reranking — are its own, never the caller's.
    cfg: Arc<crate::config::Config>,
    serve: Option<Arc<serve::ServeState>>,
    /// Bearer token required on the serving TCP listener (None = no TCP
    /// serving).
    tcp_token: Option<String>,
    /// Bearer token required on the LOCAL channel — `Some` only where the
    /// local transport is not access-controlled by the operating system.
    local_token: Option<String>,
    shutdown: AtomicBool,
}

/// Which connection a request arrived on, and therefore what it may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Channel {
    /// Local channel the operating system already gates (unix socket file
    /// mode). No credential; every op, shutdown included.
    LocalTrusted,
    /// Local channel any process on this machine can reach (loopback TCP).
    /// Token required; still local, so shutdown stays allowed.
    LocalToken,
    /// The serving listener: token required, shutdown refused.
    Remote,
}

/// The policy this platform's LOCAL channel runs under.
const LOCAL_CHANNEL: Channel =
    if ipc::LOCAL_REQUIRES_TOKEN { Channel::LocalToken } else { Channel::LocalTrusted };

impl Channel {
    /// The credential this channel demands, if any.
    fn expected_token(self, ctx: &Ctx) -> Option<Option<&String>> {
        match self {
            Channel::LocalTrusted => None,
            Channel::LocalToken => Some(ctx.local_token.as_ref()),
            Channel::Remote => Some(ctx.tcp_token.as_ref()),
        }
    }

    /// Shutdown is a LOCAL op: a serving peer must never be able to stop the
    /// daemon that answers it.
    fn allows_shutdown(self) -> bool {
        !matches!(self, Channel::Remote)
    }
}

/// Bearer check for a channel that demands one. A missing expectation and a
/// missing presentation both refuse — an unconfigured token is never an open
/// door.
fn authorized(expected: Option<&String>, presented: Option<&String>) -> bool {
    match (expected, presented) {
        (Some(expected), Some(got)) => serve::token_eq(expected, got),
        _ => false,
    }
}

/// Runs a barrier-gated query op against the committed index snapshot and
/// stamps the coherence labels (origin, generation, fresh) on the response.
/// The generation is read from the SAME connection that answers, so the
/// label always matches the snapshot.
fn serve_query(
    ctx: &Ctx,
    f: impl FnOnce(&rusqlite::Connection) -> anyhow::Result<Response>,
) -> Response {
    let Some(state) = &ctx.serve else {
        return Response::err("serving is not enabled on this daemon (config serve.enabled)");
    };
    let outcome = state.barrier(serve::BARRIER_TIMEOUT);
    let conn = match index::open_ro(state.state_dir()) {
        Ok(c) => c,
        Err(e) => return Response::err(format!("open index: {e}")),
    };
    let mut resp = match f(&conn) {
        Ok(r) => r,
        Err(e) => return Response::err(e.to_string()),
    };
    resp.ok = true;
    resp.origin = Some(state.origin.clone());
    resp.generation = Some(index::generation(&conn));
    resp.fresh = Some(outcome.fresh);
    resp.stale_note = outcome.note;
    resp.barrier_ms = Some(outcome.waited_ms);
    resp
}

fn handle(req: &Request, ctx: &Ctx) -> (Response, bool) {
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
                    let scope = resident::SessionScope::from_cwd(req.cwd.as_deref());
                    let d = resident::build(&cfg, &scope);
                    (Response { ok: true, digest: Some(d.text), ..Response::default() }, false)
                }
                Err(e) => (Response::err(e.to_string()), false),
            }
        }
        "health" => {
            let degraded = heartbeat::degraded().into_iter().map(|(n, _)| n).collect();
            (Response { ok: true, degraded_hooks: Some(degraded), ..Response::default() }, false)
        }
        "scan-code" => {
            if begin_code_scan() {
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
        "serve-status" => {
            let info = match &ctx.serve {
                Some(s) => ServeInfo {
                    enabled: true,
                    origin: s.origin.clone(),
                    generation: s.generation.load(Ordering::Relaxed),
                    last_barrier_ms: s.last_barrier_ms.load(Ordering::Relaxed),
                    bind: s.bind_addr.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone(),
                    barrier_mode: s.mode().label().to_string(),
                },
                None => ServeInfo {
                    enabled: false,
                    origin: String::new(),
                    generation: 0,
                    last_barrier_ms: 0,
                    bind: None,
                    barrier_mode: String::new(),
                },
            };
            (Response { ok: true, serve: Some(info), ..Response::default() }, false)
        }
        "recall" => {
            let query = req.query.clone().unwrap_or_default();
            let limit = req.limit.unwrap_or(8);
            let semantic = req.semantic.unwrap_or(false);
            let hybrid = req.hybrid.unwrap_or(false);
            let cfg = ctx.cfg.clone();
            (
                serve_query(ctx, |conn| {
                    let r = crate::pipeline::ranked(&cfg, conn, &query, limit, semantic, hybrid)?;
                    Ok(Response {
                        hits: Some(r.hits.into_iter().map(Into::into).collect()),
                        note: r.note,
                        ..Response::default()
                    })
                }),
                false,
            )
        }
        "expand" => {
            let cite = req.cite.clone().unwrap_or_default();
            (
                serve_query(ctx, |conn| {
                    let blocks = index::expand(conn, &cite)?;
                    Ok(Response {
                        blocks: Some(blocks.into_iter().map(Into::into).collect()),
                        ..Response::default()
                    })
                }),
                false,
            )
        }
        "find" => {
            let query = req.query.clone().unwrap_or_default();
            let limit = req.limit.unwrap_or(10);
            (
                serve_query(ctx, |conn| {
                    let hits = crate::code::find(conn, &query, limit)?;
                    Ok(Response {
                        code_hits: Some(hits.into_iter().map(Into::into).collect()),
                        ..Response::default()
                    })
                }),
                false,
            )
        }
        "map" => {
            // The repo map is a pure read over the committed catalog, so it
            // serves exactly like find: same barrier, same coherence labels.
            // (scan and embed-index stay local to the storage host — they
            // WRITE.) The code roots come from the serving host's config,
            // reloaded per request like every other config read here.
            let focus = req.focus.clone();
            let budget = req.budget_tokens.unwrap_or(crate::graph::DEFAULT_MAP_BUDGET_TOKENS);
            (
                serve_query(ctx, |conn| {
                    let cfg = Config::load()?;
                    let m = crate::graph::map(
                        conn,
                        &cfg.effective_code_roots(),
                        focus.as_deref(),
                        budget,
                    )?;
                    Ok(Response { map: Some(m.into()), ..Response::default() })
                }),
                false,
            )
        }
        "slices" => {
            // Hook advisory path: read-only, deliberately WITHOUT the
            // barrier — a hint must never cost seconds on the interactive
            // path, and a slightly stale line range is a missed optimization,
            // not wrong knowledge.
            if ctx.serve.is_none() {
                return (
                    Response::err("serving is not enabled on this daemon (config serve.enabled)"),
                    false,
                );
            }
            let path = req.path.clone().unwrap_or_default();
            let limit = req.limit.unwrap_or(5);
            let slices = hooks::symbol_slices(&paths::state_dir().join("index.db"), &path, limit);
            (Response { ok: true, slices: Some(slices), ..Response::default() }, false)
        }
        "generation" => (serve_query(ctx, |_conn| Ok(Response::default())), false),
        "checksum" => (
            serve_query(ctx, |conn| {
                Ok(Response { checksum: Some(index::catalog_checksum(conn)?), ..Response::default() })
            }),
            false,
        ),
        "shutdown" => (Response { ok: true, ..Response::default() }, true),
        other => (Response::err(format!("unknown op: {other}")), false),
    }
}

/// Serves one connection: one request line, one response line. Returns true
/// when this connection requested shutdown (local channels only).
fn serve_conn<S: Read + Write>(stream: S, ctx: &Ctx, chan: Channel) -> bool {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return false;
    }
    let (resp, shutdown) = match serde_json::from_str::<Request>(&line) {
        Ok(req) => {
            // Bearer gate wherever the transport is not access-controlled by
            // the operating system: every op requires the token.
            if let Some(expected) = chan.expected_token(ctx)
                && !authorized(expected, req.token.as_ref())
            {
                (Response::err("unauthorized"), false)
            } else if req.op == "shutdown" && !chan.allows_shutdown() {
                (Response::err("shutdown is local-only"), false)
            } else {
                let (r, shutdown) = handle(&req, ctx);
                (r, shutdown && chan.allows_shutdown())
            }
        }
        Err(e) => (Response::err(format!("bad request: {e}")), false),
    };
    let mut stream = reader.into_inner();
    if let Ok(s) = serde_json::to_string(&resp) {
        let _ = writeln!(stream, "{s}");
    }
    shutdown
}

/// Foreground run loop (systemd/`daemon start` both end up here).
pub fn run() -> anyhow::Result<()> {
    // Serving mode needs the config at boot; a corrupt config fails LOUDLY
    // here (hooks then use their daemon-less fallbacks) instead of silently
    // starting a daemon that cannot serve.
    let cfg = Config::load()?;
    let serve_handle = if cfg.serve.enabled { Some(serve::start(&cfg)?) } else { None };
    let tcp_token = match (&cfg.serve.bind, &cfg.serve.token_file) {
        (Some(_), Some(tf)) => Some(serve::read_token(tf, true)?),
        _ => None,
    };
    // Minted before anything binds so the request context can carry it
    // without reordering the boot sequence; `None` on unix.
    let local_token = ipc::new_local_token();
    let ctx = Arc::new(Ctx {
        cfg: Arc::new(cfg.clone()),
        serve: serve_handle.as_ref().map(|h| h.state.clone()),
        tcp_token: tcp_token.clone(),
        local_token: local_token.clone(),
        shutdown: AtomicBool::new(false),
    });

    if let (Some(bind), Some(_)) = (&cfg.serve.bind, &tcp_token) {
        let listener = std::net::TcpListener::bind(bind)?;
        let local = listener.local_addr()?;
        // Record the actual bound address (resolves ":0" configs) for
        // status/selfcheck and the torture harness.
        std::fs::write(paths::state_dir().join("serve.addr"), local.to_string())?;
        if let Some(h) = &serve_handle {
            *h.state.bind_addr.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(local.to_string());
        }
        eprintln!("cfetch daemon serving TCP on {local}");
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = conn.set_write_timeout(Some(Duration::from_secs(5)));
                let ctx = ctx.clone();
                std::thread::spawn(move || serve_conn(conn, &ctx, Channel::Remote));
            }
        });
    }

    // The daemon's own code-scan cadence. Detached: the listener bind below
    // must not wait for a code scan, and neither must any query.
    if let Some(h) = &serve_handle {
        let state = h.state.clone();
        let deadline = Instant::now() + FIRST_SCAN_SETTLE_WAIT;
        std::thread::spawn(move || {
            scan_cadence(
                || first_scan_ready(&state, deadline),
                begin_code_scan,
                SCAN_READY_POLL,
                serve::FINGERPRINT_INTERVAL,
                None,
            );
        });
    }

    // A stale endpoint from a dead daemon is cleared; a live one is never
    // stolen (see `ipc::listen`).
    let listener = ipc::listen(local_token)?;
    eprintln!("cfetch daemon listening on {}", listener.describe());
    for conn in listener.incoming() {
        if ctx.shutdown.load(Ordering::SeqCst) {
            break;
        }
        let Ok(conn) = conn else { continue };
        let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = conn.set_write_timeout(Some(Duration::from_secs(5)));
        // Thread per connection: a query blocked in the drain barrier (up to
        // 5s) must not starve other clients — hooks poll this channel on the
        // interactive path.
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            if serve_conn(conn, &ctx, LOCAL_CHANNEL) {
                ctx.shutdown.store(true, Ordering::SeqCst);
                // Wake the accept loop so it observes the flag.
                ipc::wake();
            }
        });
    }
    listener.cleanup();
    Ok(())
}

/// Detached start: re-executes this binary with `daemon run`.
pub fn start() -> anyhow::Result<()> {
    if call("ping", Duration::from_millis(300)).is_some() {
        println!("daemon already running");
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["daemon", "run"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: the daemon outlives
        // the console that started it and never receives its Ctrl-C. Closing
        // stdio is enough on unix; on Windows the console is inherited
        // separately from the handles.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn()?;
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(50));
        if call("ping", Duration::from_millis(200)).is_some() {
            println!("daemon started on {}", ipc::describe());
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
    // The mode LEADS: which side of the serving topology this host is on is
    // the first thing an operator needs, not a footnote under the ledger.
    let cfg = Config::load().ok();
    let info = call("serve-status", Duration::from_millis(300)).and_then(|r| r.serve);
    match &cfg {
        Some(c) => println!("{}", mode_line(c, info.as_ref())),
        None => println!("mode: unknown (config does not load)"),
    }
    match call("ping", Duration::from_millis(300)) {
        Some(r) => {
            println!("daemon: running (v{})", r.version.unwrap_or_default());
            if let Some(info) = info.filter(|i| i.enabled) {
                println!(
                    "serving: origin {}, generation {}, drain barrier {}, last barrier {} ms, {}",
                    info.origin,
                    info.generation,
                    barrier_mode_of(&info),
                    info.last_barrier_ms,
                    info.bind
                        .clone()
                        .map_or_else(|| "local channel only".to_string(), |b| format!("tcp {b}"))
                );
            }
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
        None => println!("daemon: not running ({})", ipc::describe()),
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
    fn resident_request_carries_the_session_cwd() {
        // The daemon shares the host with its caller but not the working
        // directory; without this field every scoped entry would be decided
        // against the daemon's own cwd.
        let req: Request =
            serde_json::from_str(r#"{"op":"resident","cwd":"/srv/work/widget"}"#).unwrap();
        assert_eq!(req.cwd.as_deref(), Some("/srv/work/widget"));
        let scope = resident::SessionScope::from_cwd(req.cwd.as_deref());
        assert_eq!(scope.repo.as_deref(), Some("widget"));

        // An older client sends no cwd at all: host-scoped entries still
        // resolve, repo-scoped ones simply do not match.
        let old: Request = serde_json::from_str(r#"{"op":"resident"}"#).unwrap();
        assert!(old.cwd.is_none());
        assert!(resident::SessionScope::from_cwd(None).repo.is_none());
    }

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

    fn no_serve_ctx() -> Ctx {
        Ctx {
            cfg: Arc::new(crate::config::Config::default()),
            serve: None,
            tcp_token: None,
            local_token: None,
            shutdown: AtomicBool::new(false),
        }
    }

    #[test]
    fn daemon_start_kicks_exactly_one_code_scan() {
        // A fabricated daemon start: watch registration takes two polls, then
        // the cadence runs two rounds. Round one begins THE scan (nobody sent
        // `scan-code` — the daemon kicks itself); round two is refused because
        // that scan is still running. Single flight, one scan.
        use std::sync::atomic::AtomicUsize;
        let polls = AtomicUsize::new(0);
        let coord = ScanCoordinator::new();
        let kicked = AtomicUsize::new(0);
        let begun = AtomicUsize::new(0);
        scan_cadence(
            || polls.fetch_add(1, Ordering::SeqCst) >= 2,
            || {
                kicked.fetch_add(1, Ordering::SeqCst);
                let started = coord.try_begin();
                if started {
                    begun.fetch_add(1, Ordering::SeqCst);
                }
                started
            },
            Duration::from_millis(1),
            Duration::from_millis(1),
            Some(2),
        );
        assert!(
            polls.load(Ordering::SeqCst) >= 3,
            "the cadence must wait for watch registration before scanning"
        );
        assert_eq!(kicked.load(Ordering::SeqCst), 2, "the cadence repeats on the fingerprint tick");
        assert_eq!(
            begun.load(Ordering::SeqCst),
            1,
            "single flight: a second scan must not start while the first runs"
        );
        assert!(coord.status().running);
    }

    #[test]
    fn the_first_code_scan_waits_for_the_tree_index_to_settle() {
        let dir = tempfile::tempdir().unwrap();
        let state = serve::ServeState::new(
            "o".to_string(),
            dir.path().to_path_buf(),
            dir.path().join("barrier"),
        );
        let far = Instant::now() + Duration::from_secs(300);
        assert!(!first_scan_ready(&state, far), "nothing is ready at startup");
        state.mark_watches_ready();
        assert!(
            !first_scan_ready(&state, far),
            "watches alone must not start a cold code scan against the tree index's writer"
        );
        state.mark_applied(0, 1);
        assert!(first_scan_ready(&state, far), "a settled tree index releases the code scan");

        // A tree index that never settles must not mean a code index that
        // never exists: the bounded wait releases the scan anyway.
        let stuck = serve::ServeState::new(
            "o".to_string(),
            dir.path().to_path_buf(),
            dir.path().join("barrier"),
        );
        let past = Instant::now() - Duration::from_secs(1);
        assert!(!first_scan_ready(&stuck, past), "watches are still the floor");
        stuck.mark_watches_ready();
        assert!(first_scan_ready(&stuck, past));
    }

    #[test]
    fn status_states_the_serving_mode_in_one_line() {
        let mut cfg = Config::default();
        cfg.serve.enabled = true;
        cfg.serve.origin = Some("storage-1".to_string());
        let info = ServeInfo {
            enabled: true,
            origin: "storage-1".to_string(),
            generation: 42,
            last_barrier_ms: 3,
            bind: Some("198.51.100.7:9737".to_string()),
            barrier_mode: serve::BarrierMode::Unordered.label().to_string(),
        };
        let line = mode_line(&cfg, Some(&info));
        assert!(line.starts_with("mode: serving host storage-1"), "{line}");
        assert!(line.contains("generation 42"), "{line}");
        assert!(line.contains("198.51.100.7:9737"), "{line}");
        // The operator must be able to READ which coherence path is in force.
        assert!(line.contains("barrier unordered (fingerprint)"), "{line}");
        // A serving host too old to report one must not read as the fast path.
        let unreported = ServeInfo { barrier_mode: String::new(), ..info.clone() };
        assert!(
            mode_line(&cfg, Some(&unreported)).contains("barrier mode not reported"),
            "an unreported mode must never imply a guarantee"
        );
        assert_eq!(line.lines().count(), 1, "the mode must be ONE line: {line}");
        // Daemon down: still obviously a serving host, generation unknown.
        let down = mode_line(&cfg, None);
        assert!(down.starts_with("mode: serving host storage-1"), "{down}");
        assert!(down.contains("daemon"), "{down}");
    }

    #[test]
    fn status_states_the_none_tier_mode_in_one_line() {
        let mut cfg = Config::default();
        cfg.client.serving = Some(crate::config::ClientServingConfig {
            addr: "198.51.100.7:9737".to_string(),
            token_file: std::path::PathBuf::from("/var/empty/token"),
        });
        let line = mode_line(&cfg, None);
        assert!(line.starts_with("mode: none-tier"), "{line}");
        assert!(line.contains("198.51.100.7:9737"), "the serving address must be on the line: {line}");
        assert_eq!(line.lines().count(), 1, "the mode must be ONE line: {line}");
        // A host with neither role still says which mode it is in.
        let plain = mode_line(&Config::default(), None);
        assert!(plain.starts_with("mode: local"), "{plain}");
    }

    #[test]
    fn query_ops_refuse_when_serving_disabled() {
        let ctx = no_serve_ctx();
        for op in ["recall", "expand", "find", "map", "slices", "generation", "checksum"] {
            let (resp, shutdown) =
                handle(&Request { op: op.to_string(), ..Request::default() }, &ctx);
            assert!(!resp.ok, "{op} must refuse without serve.enabled");
            assert!(resp.error.unwrap().contains("serve.enabled"), "{op} error must name the fix");
            assert!(!shutdown);
        }
    }

    #[test]
    fn serve_status_reports_disabled_without_serving() {
        let ctx = no_serve_ctx();
        let (resp, _) = handle(&Request { op: "serve-status".into(), ..Request::default() }, &ctx);
        assert!(resp.ok);
        assert!(!resp.serve.unwrap().enabled);
    }

    /// In-memory duplex "stream" for serve_conn tests.
    struct Duplex {
        input: std::io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Read for Duplex {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buf)
        }
    }
    impl Write for Duplex {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.output.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn roundtrip(ctx: &Ctx, chan: Channel, body: serde_json::Value) -> Response {
        let mut stream = Duplex { input: std::io::Cursor::new(format!("{body}\n").into_bytes()), output: Vec::new() };
        serve_conn(&mut stream, ctx, chan);
        serde_json::from_slice(&stream.output).unwrap()
    }

    /// Did this connection ask for shutdown, and was it granted?
    fn roundtrip_shutdown(ctx: &Ctx, chan: Channel, body: serde_json::Value) -> bool {
        let mut stream = Duplex { input: std::io::Cursor::new(format!("{body}\n").into_bytes()), output: Vec::new() };
        serve_conn(&mut stream, ctx, chan)
    }

    #[test]
    fn tcp_requires_the_bearer_token_for_every_op() {
        let ctx = Ctx {
            cfg: Arc::new(crate::config::Config::default()),
            serve: None,
            tcp_token: Some("right-token".to_string()),
            local_token: None,
            shutdown: AtomicBool::new(false),
        };
        let r = roundtrip(&ctx, Channel::Remote, serde_json::json!({"op": "ping"}));
        assert!(!r.ok, "missing token must be refused");
        assert_eq!(r.error.as_deref(), Some("unauthorized"));
        let r = roundtrip(&ctx, Channel::Remote, serde_json::json!({"op": "ping", "token": "wrong-token"}));
        assert_eq!(r.error.as_deref(), Some("unauthorized"));
        let r = roundtrip(&ctx, Channel::Remote, serde_json::json!({"op": "ping", "token": "right-token"}));
        assert!(r.ok);
    }

    #[test]
    fn the_ranking_mode_and_its_note_cross_the_wire_in_both_directions() {
        // A client asks the SERVING host to rank semantically, because a
        // none-tier host has no vectors and no endpoint of its own.
        let r: Request =
            serde_json::from_str(r#"{"op":"recall","query":"q","limit":3,"semantic":true}"#).unwrap();
        assert_eq!(r.semantic, Some(true));
        assert_eq!(r.hybrid, None);
        // An older client that never learned the flags still parses, and asks
        // for exactly what it always asked for.
        let r: Request = serde_json::from_str(r#"{"op":"recall","query":"q"}"#).unwrap();
        assert_eq!((r.semantic, r.hybrid), (None, None));
        // And an older SERVING host's answer, which carries no note, is not a
        // parse failure on a newer client.
        let resp: Response = serde_json::from_str(r#"{"ok":true}"#).unwrap();
        assert!(resp.note.is_none());
    }

    #[test]
    fn shutdown_is_local_only() {
        let ctx = Ctx {
            cfg: Arc::new(crate::config::Config::default()),
            serve: None,
            tcp_token: Some("t".to_string()),
            local_token: None,
            shutdown: AtomicBool::new(false),
        };
        let r = roundtrip(&ctx, Channel::Remote, serde_json::json!({"op": "shutdown", "token": "t"}));
        assert!(!r.ok);
        assert!(r.error.unwrap().contains("local-only"));
        assert!(
            !roundtrip_shutdown(&ctx, Channel::Remote, serde_json::json!({"op": "shutdown", "token": "t"})),
            "a refused shutdown must not stop the daemon either"
        );
    }

    // ---- local channel policy, per transport ----
    //
    // Both local policies are exercised on every platform: the Windows local
    // channel is loopback TCP, and its gate must be provable on the runner
    // that can actually run these tests.

    #[test]
    fn an_os_gated_local_channel_needs_no_token_and_may_shut_down() {
        // The unix socket: its file mode IS the access control.
        let ctx = no_serve_ctx();
        let r = roundtrip(&ctx, Channel::LocalTrusted, serde_json::json!({"op": "ping"}));
        assert!(r.ok, "a unix-socket client presents no credential: {r:?}");
        assert!(roundtrip_shutdown(&ctx, Channel::LocalTrusted, serde_json::json!({"op": "shutdown"})));
    }

    #[test]
    fn a_loopback_local_channel_needs_the_token_and_may_still_shut_down() {
        // Loopback TCP (Windows): any local process can connect, so the
        // daemon's own token gates it — but it is still a LOCAL channel, so
        // `daemon stop` must keep working.
        let ctx = Ctx {
            cfg: Arc::new(crate::config::Config::default()),
            serve: None,
            tcp_token: None,
            local_token: Some("local-token".to_string()),
            shutdown: AtomicBool::new(false),
        };
        let r = roundtrip(&ctx, Channel::LocalToken, serde_json::json!({"op": "ping"}));
        assert_eq!(r.error.as_deref(), Some("unauthorized"), "no token: refused");
        let r = roundtrip(&ctx, Channel::LocalToken, serde_json::json!({"op": "ping", "token": "guess"}));
        assert_eq!(r.error.as_deref(), Some("unauthorized"), "wrong token: refused");
        let r = roundtrip(&ctx, Channel::LocalToken, serde_json::json!({"op": "ping", "token": "local-token"}));
        assert!(r.ok, "the published token opens the local channel: {r:?}");
        assert!(
            roundtrip_shutdown(&ctx, Channel::LocalToken, serde_json::json!({"op": "shutdown", "token": "local-token"})),
            "shutdown is a local op on every transport"
        );
    }

    #[test]
    fn a_token_gated_channel_without_a_configured_token_refuses_everything() {
        // An unconfigured credential is never an open door.
        let ctx = no_serve_ctx();
        for chan in [Channel::LocalToken, Channel::Remote] {
            let r = roundtrip(&ctx, chan, serde_json::json!({"op": "ping", "token": "anything"}));
            assert_eq!(r.error.as_deref(), Some("unauthorized"), "{chan:?}");
            let r = roundtrip(&ctx, chan, serde_json::json!({"op": "ping"}));
            assert_eq!(r.error.as_deref(), Some("unauthorized"), "{chan:?}");
        }
    }

    #[test]
    fn this_platforms_local_channel_matches_its_transport() {
        assert_eq!(
            LOCAL_CHANNEL,
            if cfg!(windows) { Channel::LocalToken } else { Channel::LocalTrusted }
        );
        assert!(LOCAL_CHANNEL.allows_shutdown(), "the local channel always accepts shutdown");
    }
}
