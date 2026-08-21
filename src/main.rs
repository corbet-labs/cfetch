mod audit;
mod code;
mod config;
mod daemon;
mod dashboard;
mod embed;
mod exhaust;
mod govern;
mod graph;
mod heartbeat;
mod hook_io;
mod hooks;
mod index;
mod install;
mod ledger;
mod lockfile;
mod markers;
mod mcp;
mod paths;
mod resident;
mod session_state;
mod transcript;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "cfetch", version, about = "A second brain for coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a hook entrypoint (invoked by the agent harness, reads stdin)
    Hook {
        /// session-start | user-prompt | pre-tool | post-tool | stop | precompact
        event: String,
    },
    /// Manage the per-host daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Register (or remove) cfetch's hooks in Claude Code settings
    Install {
        /// Path to settings.json (default: ~/.claude/settings.json)
        #[arg(long)]
        settings: Option<std::path::PathBuf>,
        /// Remove cfetch's managed entries instead of adding them
        #[arg(long)]
        remove: bool,
    },
    /// Rebuild the recall index and sync the code index
    Scan {
        /// Hand the (slow) code scan to the daemon's background thread;
        /// falls back inline with a warning when the daemon is unreachable
        #[arg(long = "async")]
        background: bool,
    },
    /// Locate a symbol or file in the code index, with exact line ranges
    Find {
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Print a repo map fitted to a token budget, ordered by import-graph importance
    Map {
        /// Personalize the ranking toward files whose path or symbols match this term
        #[arg(long)]
        focus: Option<String>,
        /// Token budget the rendered map must fit
        #[arg(long, default_value_t = 1500)]
        budget_tokens: u64,
    },
    /// Search the brain (rings 0-4), BM25-ranked, with ring-prefixed citations
    Recall {
        /// Search terms (word-prefix matched, OR-combined)
        query: Vec<String>,
        /// Expand a citation id instead of searching
        #[arg(long)]
        id: Option<String>,
        /// Also list docs wikilinked to the top hits (1-hop curated graph)
        #[arg(long)]
        expand: bool,
        /// Rank purely by embedding cosine similarity (requires embeddings config)
        #[arg(long, conflicts_with = "hybrid")]
        semantic: bool,
        /// Fuse BM25 and semantic rankings via reciprocal rank fusion
        #[arg(long)]
        hybrid: bool,
        #[arg(long, default_value_t = 8)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Review ring-5 staging: auto-flagged exhaust (local only, never injected)
    Staging {
        #[command(subcommand)]
        action: StagingAction,
    },
    /// Embed index blocks lacking vectors (resumable; requires embeddings config)
    EmbedIndex {
        /// Blocks per embeddings request
        #[arg(long, default_value_t = 64)]
        batch: usize,
    },
    /// Open the terminal dashboard: health, ledger, live recall
    Dashboard,
    /// Serve recall/find/expand over MCP (stdio) for any MCP client
    Mcp,
    /// Verify the installation end to end; nonzero exit on hard failures
    Selfcheck,
    /// Price the always-on context bill: CLAUDE.md + imports, MCP servers,
    /// booked injection vs budget, position-weighted cost, measurement gaps
    Audit {
        #[arg(long)]
        json: bool,
    },
    /// Show daemon, hook health, and state footprint
    Status,
}

#[derive(Subcommand)]
enum StagingAction {
    /// List flagged, unconsumed candidates, newest first
    List {
        #[arg(long)]
        json: bool,
    },
    /// Mark a candidate consumed (a distillation session has taken it)
    Consume { id: i64 },
    /// Drop a candidate from staging without consuming it
    Dismiss { id: i64 },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Run in the foreground (what systemd should call)
    Run,
    /// Start detached
    Start,
    /// Stop a running daemon
    Stop,
    /// Show whether the daemon answers
    Status,
}

fn selfcheck() -> anyhow::Result<()> {
    let mut hard_failures = 0;

    let cfg = match config::Config::load() {
        Ok(c) => {
            println!("ok    config loads ({} resident entries)", c.resident.len());
            Some(c)
        }
        Err(e) => {
            println!("FAIL  config: {e}");
            hard_failures += 1;
            None
        }
    };

    if let Some(cfg) = &cfg {
        if cfg.brain_root.is_dir() {
            println!("ok    brain root {}", cfg.brain_root.display());
        } else {
            println!("FAIL  brain root missing: {}", cfg.brain_root.display());
            hard_failures += 1;
        }
        let digest = resident::build(cfg);
        if digest.text.is_empty() {
            println!("warn  resident digest is empty — nothing will be injected");
        } else {
            println!(
                "ok    resident digest builds ({} chars, ~{} tokens estimated)",
                digest.text.len(),
                hook_io::estimate_tokens(digest.text.len())
            );
            for (label, chars) in &digest.sources {
                println!("        {label}: {chars} chars");
            }
        }
    }

    let state = paths::state_dir();
    match std::fs::create_dir_all(&state) {
        Ok(()) => println!("ok    state dir writable: {}", state.display()),
        Err(e) => {
            println!("FAIL  state dir {}: {e}", state.display());
            hard_failures += 1;
        }
    }

    match daemon::call("ping", std::time::Duration::from_millis(300)) {
        Some(r) => println!("ok    daemon answers (v{})", r.version.unwrap_or_default()),
        None => println!("warn  daemon not running — hooks fall back to direct reads"),
    }

    let degraded = heartbeat::degraded();
    if degraded.is_empty() {
        println!("ok    no degraded hooks");
    } else {
        for (name, h) in &degraded {
            println!("warn  hook {name} failing ({} consecutive)", h.consecutive_failures);
        }
    }

    if hard_failures > 0 {
        anyhow::bail!("{hard_failures} hard failure(s)");
    }
    Ok(())
}

fn scan(background: bool) -> anyhow::Result<()> {
    let cfg = config::Config::load()?;
    {
        let mut conn = index::open(&paths::state_dir())?;
        let native = paths::native_projects_root();
        let report = index::scan(&mut conn, &cfg.brain_root, Some(&native))?;
        println!(
            "indexed {} docs, {} blocks ({} file(s) skipped as ring 5+)",
            report.docs, report.blocks, report.skipped_high_ring
        );
        // Name the skipped files (a malformed ring declaration also lands
        // here, fail-closed) so a quarantined-by-accident file is visible.
        if !report.skipped.is_empty() {
            let shown: Vec<&str> = report.skipped.iter().take(10).map(String::as_str).collect();
            let more = report.skipped.len() - shown.len();
            println!(
                "  skipped (ring 5+ or unparseable ring frontmatter): {}{}",
                shown.join(", "),
                if more > 0 { format!(" … and {more} more") } else { String::new() }
            );
        }
        // Connection dropped here: the daemon's scan thread is the next writer.
    }
    if background {
        match daemon::call("scan-code", std::time::Duration::from_secs(1)) {
            Some(r) if r.ok => {
                println!("code scan running in the daemon (watch: cfetch status)");
                return Ok(());
            }
            Some(r) => {
                println!("daemon refused: {}", r.error.unwrap_or_else(|| "unknown error".into()));
                return Ok(());
            }
            None => println!("warning: daemon unreachable — running the code scan inline"),
        }
    }
    let mut conn = index::open(&paths::state_dir())?;
    let code = code::scan_code(&mut conn, &cfg.effective_code_roots())?;
    println!(
        "code: {} files, {} symbols (re)parsed, {} import edges",
        code.files, code.symbols, code.edges
    );
    Ok(())
}

fn find(query: &str, limit: usize, json: bool) -> anyhow::Result<()> {
    // Serves the existing snapshot ONLY: a full code scan over NFS roots
    // takes minutes and must never ride on an interactive lookup. `cfetch
    // scan` (or the daemon's background scan) owns index freshness.
    let conn = index::open(&paths::state_dir())?;
    let hits = code::find(&conn, query, limit)?;
    if let Some(r) = daemon::call("scan-status", std::time::Duration::from_millis(250))
        && let Some(s) = r.scan
        && s.running
    {
        // stderr so --json stdout stays parseable.
        eprintln!("note: a code scan is running — results may be stale");
    }
    if json {
        let arr: Vec<_> = hits
            .iter()
            .map(|h| {
                serde_json::json!({
                    "path": h.path, "name": h.name, "kind": h.kind,
                    "lines": [h.start_line, h.end_line], "tokens_estimated": h.token_estimate,
                    "rank_pct": h.rank_pct,
                })
            })
            .collect();
        println!("{}", serde_json::json!(arr));
        return Ok(());
    }
    if hits.is_empty() {
        if code::file_count(&conn)? == 0 {
            println!("code index is empty — run `cfetch scan` (or `cfetch scan --async`)");
        } else {
            println!("no hits for \"{query}\"");
        }
        return Ok(());
    }
    for h in &hits {
        match (&h.name, &h.kind) {
            (Some(name), Some(kind)) => println!(
                "{}:{}-{}  {} {}  (~{} tok)",
                h.path, h.start_line, h.end_line, kind, name, h.token_estimate
            ),
            _ => println!("{}  (file match)", h.path),
        }
    }
    Ok(())
}

fn map_cmd(focus: Option<&str>, budget_tokens: u64) -> anyhow::Result<()> {
    let cfg = config::Config::load()?;
    let conn = index::open(&paths::state_dir())?;
    let m = graph::map(&conn, &cfg.effective_code_roots(), focus, budget_tokens)?;
    if m.lines.is_empty() {
        println!("code index is empty — run `cfetch scan` first");
        return Ok(());
    }
    if focus.is_some() && !m.focus_matched {
        eprintln!("note: --focus matched no path or symbol — showing the unpersonalized map");
    }
    for line in &m.lines {
        println!("{line}");
    }
    if m.lines.len() < m.total_files {
        println!("… {} more file(s) beyond the token budget", m.total_files - m.lines.len());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)] // thin CLI adapter, mirrors the flag set
fn recall(
    query: &str,
    id: Option<&str>,
    expand: bool,
    semantic: bool,
    hybrid: bool,
    limit: usize,
    json: bool,
) -> anyhow::Result<()> {
    let cfg = config::Config::load()?;
    let native = paths::native_projects_root();
    let conn = index::ensure_fresh(&paths::state_dir(), &cfg.brain_root, Some(&native))?;

    if let Some(cite) = id {
        let blocks = index::expand(&conn, cite)?;
        if blocks.is_empty() {
            println!("no block with citation {cite} (index may have moved on — content-addressed ids change when the entry changes)");
            return Ok(());
        }
        for b in blocks {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "cite": b.cite, "path": b.path, "ring": b.ring,
                        "lines": [b.start_line, b.end_line], "text": b.text,
                    })
                );
            } else {
                println!("{} {}:{}-{} (ring {})\n{}\n", b.cite, b.path, b.start_line, b.end_line, b.ring, b.text);
            }
        }
        return Ok(());
    }

    if query.trim().is_empty() {
        anyhow::bail!("empty query (pass search terms or --id <citation>)");
    }
    let hits = if semantic || hybrid {
        embed::semantic_hits(&cfg, &conn, query, limit, hybrid)?
    } else {
        index::recall(&conn, query, limit)?
    };
    let linked = if expand && !hits.is_empty() {
        let top: Vec<String> = hits.iter().take(3).map(|h| h.path.clone()).collect();
        index::linked_docs(&conn, &top, 8)?
    } else {
        Vec::new()
    };
    if json {
        let arr: Vec<_> = hits
            .iter()
            .map(|h| {
                serde_json::json!({
                    "cite": h.cite, "path": h.path, "ring": h.ring,
                    "lines": [h.start_line, h.end_line], "snippet": h.snippet,
                    "mirrors": h.mirrors,
                })
            })
            .collect();
        let links: Vec<_> = linked
            .iter()
            .map(|(p, r)| serde_json::json!({"path": p, "ring": r}))
            .collect();
        println!("{}", serde_json::json!({"hits": arr, "linked": links}));
    } else if hits.is_empty() {
        println!("no hits for \"{query}\"");
    } else {
        for h in &hits {
            println!("{} {}:{}-{} (ring {})", h.cite, h.path, h.start_line, h.end_line, h.ring);
            println!("    {}", h.snippet);
            if !h.mirrors.is_empty() {
                println!("    (also at: {})", h.mirrors.join(", "));
            }
        }
        if !linked.is_empty() {
            println!("\nlinked (curated wikilinks, 1 hop from top hits):");
            for (p, r) in &linked {
                println!("    {p} (ring {r})");
            }
        }
        println!("\nexpand a hit: cfetch recall --id <citation>");
    }
    Ok(())
}

/// Ring-5 staging review. Read side only: flagging happens in the Stop hook's
/// traps. Nothing here is ever injected into a session or synced off-host.
fn staging(action: StagingAction) -> anyhow::Result<()> {
    let conn = exhaust::open(&paths::state_dir())?;
    match action {
        StagingAction::List { json } => {
            let rows = exhaust::staging_list(&conn)?;
            if json {
                let arr: Vec<_> = rows
                    .iter()
                    .map(|r| {
                        let payload: serde_json::Value = serde_json::from_str(&r.payload)
                            .unwrap_or_else(|_| serde_json::Value::String(r.payload.clone()));
                        serde_json::json!({
                            "id": r.id, "reason": r.reason, "kind": r.kind,
                            "session_id": r.session_id, "ts": r.ts, "payload": payload,
                        })
                    })
                    .collect();
                println!("{}", serde_json::json!(arr));
            } else if rows.is_empty() {
                println!("staging is empty — no flagged exhaust awaiting review");
            } else {
                for r in &rows {
                    let session = r.session_id.get(..8).unwrap_or(&r.session_id);
                    println!("#{}  {}  {}  session {}  {}", r.id, r.reason, r.kind, session, r.payload);
                }
                println!("\ndistill a candidate, then: cfetch staging consume <id> | dismiss <id>");
            }
        }
        StagingAction::Consume { id } => {
            if exhaust::consume(&conn, id)? {
                println!("consumed #{id}");
            } else {
                anyhow::bail!("no staged candidate #{id} (never flagged, or already consumed/dismissed)");
            }
        }
        StagingAction::Dismiss { id } => {
            if exhaust::dismiss(&conn, id)? {
                println!("dismissed #{id}");
            } else {
                anyhow::bail!("no staged candidate #{id} (never flagged, or already consumed/dismissed)");
            }
        }
    }
    Ok(())
}

fn audit_cmd(json: bool) -> anyhow::Result<()> {
    let cfg = config::Config::load()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let report = audit::build(&audit::AuditPaths::defaults(), cfg.budget_chars, now);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", audit::render(&report));
    }
    Ok(())
}

fn status() -> anyhow::Result<()> {
    daemon::status()?;
    let quarantines = ledger::quarantine_count(&paths::state_dir());
    if quarantines > 0 {
        println!("ledger: {quarantines} quarantined corrupt file(s) — torn writes occurred (ledger.json.corrupt-*)");
    }
    let ledger = ledger::load();
    let sessions = ledger.sessions.len();
    let injected: u64 = ledger
        .sessions
        .values()
        .flat_map(|s| s.by_source.values())
        .map(|t| t.tokens_estimated)
        .sum();
    println!("ledger: {sessions} session(s)");
    println!("  estimated: ~{injected} tokens injected by cfetch (chars/3.5 heuristic)");
    // Measured truth from transcripts, side by side with the estimate — and
    // clearly labeled absent when the transcript could not be parsed.
    let mut measured = ledger::MeasuredUsage::default();
    let mut measured_sessions = 0usize;
    for s in ledger.sessions.values() {
        if !s.measured.is_zero() {
            measured_sessions += 1;
            measured.accumulate(&s.measured);
        }
    }
    if measured_sessions > 0 {
        println!(
            "  measured:  {} api call(s) over {measured_sessions} session(s): {} input / {} output / {} cache-read / {} cache-created tokens (transcript ground truth)",
            measured.api_calls,
            measured.input_tokens,
            measured.output_tokens,
            measured.cache_read_input_tokens,
            measured.cache_creation_input_tokens
        );
    } else {
        println!("  measured:  none — no transcript usage booked yet, numbers above are estimates");
    }
    // Transcript-VERIFIED delivery: did our hook output actually enter the
    // conversation? Read from the newest transcript, never assumed — and a
    // drifted format is reported as unverifiable, never as zero.
    let transcripts_root = paths::native_projects_root();
    match transcript::newest_transcript(&transcripts_root) {
        None => println!(
            "delivery: no transcripts found under {} (measurement gap)",
            transcripts_root.display()
        ),
        Some(t) => match transcript::verified_injections(&t) {
            Some((fired, delivered)) => println!(
                "delivery: {fired} hook firing(s) observed, {delivered} injection(s) verified (transcript)"
            ),
            None => println!("delivery: unverifiable (transcript format drift)"),
        },
    }
    let state = paths::state_dir();
    let bytes: u64 = std::fs::read_dir(&state)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0);
    println!("state:  {} ({} KiB)", state.display(), bytes / 1024);
    Ok(())
}

fn main() {
    // Hook invocations bypass clap entirely: a parse failure would exit 2,
    // which is the harness's BLOCKING code (a Stop hook exiting 2 traps the
    // session in a loop). Hooks must reach hooks::run no matter what.
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("hook") {
        let event = argv.get(2).cloned().unwrap_or_default();
        hooks::run(&event);
        return;
    }
    let cli = Cli::parse();
    match cli.command {
        Command::Hook { event } => {
            // Unreachable in practice (pre-dispatch above), kept for --help.
            hooks::run(&event);
        }
        Command::Daemon { action } => {
            let result = match action {
                DaemonAction::Run => daemon::run(),
                DaemonAction::Start => daemon::start(),
                DaemonAction::Stop => daemon::stop(),
                DaemonAction::Status => daemon::status(),
            };
            if let Err(e) = result {
                eprintln!("cfetch daemon: {e}");
                std::process::exit(1);
            }
        }
        Command::Install { settings, remove } => {
            let path = settings.unwrap_or_else(install::default_settings_path);
            if let Err(e) = install::apply(&path, remove) {
                eprintln!("cfetch install: {e}");
                std::process::exit(1);
            }
            if !remove
                && let Err(e) = install::install_agents()
            {
                eprintln!("cfetch install (other agents): {e}");
                std::process::exit(1);
            }
        }
        Command::Staging { action } => {
            if let Err(e) = staging(action) {
                eprintln!("cfetch staging: {e}");
                std::process::exit(1);
            }
        }
        Command::Dashboard => {
            if let Err(e) = dashboard::run() {
                eprintln!("cfetch dashboard: {e}");
                std::process::exit(1);
            }
        }
        Command::Mcp => {
            if let Err(e) = mcp::serve() {
                eprintln!("cfetch mcp: {e}");
                std::process::exit(1);
            }
        }
        Command::Scan { background } => {
            if let Err(e) = scan(background) {
                eprintln!("cfetch scan: {e}");
                std::process::exit(1);
            }
        }
        Command::Find { query, limit, json } => {
            if let Err(e) = find(&query, limit, json) {
                eprintln!("cfetch find: {e}");
                std::process::exit(1);
            }
        }
        Command::Map { focus, budget_tokens } => {
            if let Err(e) = map_cmd(focus.as_deref(), budget_tokens) {
                eprintln!("cfetch map: {e}");
                std::process::exit(1);
            }
        }
        Command::Recall { query, id, expand, semantic, hybrid, limit, json } => {
            if let Err(e) = recall(&query.join(" "), id.as_deref(), expand, semantic, hybrid, limit, json) {
                eprintln!("cfetch recall: {e}");
                std::process::exit(1);
            }
        }
        Command::EmbedIndex { batch } => {
            if let Err(e) = embed::embed_index_cmd(batch) {
                eprintln!("cfetch embed-index: {e}");
                std::process::exit(1);
            }
        }
        Command::Selfcheck => {
            if let Err(e) = selfcheck() {
                eprintln!("cfetch selfcheck: {e}");
                std::process::exit(1);
            }
        }
        Command::Audit { json } => {
            if let Err(e) = audit_cmd(json) {
                eprintln!("cfetch audit: {e}");
                std::process::exit(1);
            }
        }
        Command::Status => {
            if let Err(e) = status() {
                eprintln!("cfetch status: {e}");
                std::process::exit(1);
            }
        }
    }
}
