mod audit;
mod code;
mod config;
mod daemon;
mod dashboard;
mod embed;
mod engine;
mod exhaust;
mod govern;
mod grant;
mod graph;
mod hardware;
mod heartbeat;
mod hook_io;
mod hooks;
mod index;
mod install;
mod ipc;
mod jsonl;
mod ledger;
mod lockfile;
mod markers;
mod mcp;
mod migrate;
mod net;
mod paths;
mod pipeline;
mod rerank;
mod resident;
mod serve;
mod session_state;
#[cfg(test)]
mod testhttp;
mod staging;
mod transcript;
mod vectors;

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
    /// Register (or remove) cfetch in Claude Code settings and every other
    /// detected agent (Codex, Gemini): hooks, MCP servers, instruction blocks
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
        #[arg(long, default_value_t = graph::DEFAULT_MAP_BUDGET_TOKENS)]
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
        /// Restrict to one slice and everything nested inside it
        #[arg(long)]
        slice: Option<String>,
        #[arg(long, default_value_t = 8)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Mint a one-time invite to a slice, to paste to another host
    Invite {
        /// Slice to grant (must be configured on this host)
        slice: String,
        /// Access to grant: ro or rw
        #[arg(long, default_value = "ro")]
        mode: String,
        /// Expire the invite after this many hours
        #[arg(long)]
        expires_in_hours: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Redeem an invite minted by another host
    Join {
        /// The ticket text from `cfetch invite`
        ticket: String,
        #[arg(long)]
        json: bool,
    },
    /// Show who has been granted which slice, and which invites are unused
    Grants {
        #[arg(long)]
        json: bool,
    },
    /// Detect accelerators and name the variant this machine should run
    Hardware {
        #[arg(long)]
        json: bool,
    },
    /// Show this host's network identity (created on first use)
    Identity {
        #[arg(long)]
        json: bool,
    },
    /// List the configured slices with what each one holds
    Slices {
        #[arg(long)]
        json: bool,
    },
    /// Review ring-5 staging: auto-flagged exhaust in the tree, shared across
    /// hosts, never injected
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
    Consume { id: String },
    /// Move a candidate out of staging without promoting it
    Dismiss { id: String },
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
        let scope = resident::SessionScope::current();
        let digest = resident::build(cfg, &scope);
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
        if !digest.skipped_by_scope.is_empty() {
            println!(
                "ok    {} resident entr(ies) out of scope for host {}{}",
                digest.skipped_by_scope.len(),
                scope.host,
                scope.repo.as_deref().map(|r| format!(" / repo {r}")).unwrap_or_default(),
            );
            for label in &digest.skipped_by_scope {
                println!("        {label}: not injected here");
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

    if let Some(cfg) = &cfg {
        // Rings 5 and 6 are records in the TREE, not per-host state. Report
        // where they are without creating them: a diagnostic must not be the
        // thing that first writes into the brain.
        let logs = paths::logs_dir(&cfg.brain_root);
        let staging = paths::staging_dir(&cfg.brain_root);
        let host = paths::host_id();
        println!(
            "ok    ring-6 exhaust + ledger: {} (this host writes as {host})",
            logs.display()
        );
        println!(
            "ok    ring-5 staging: {} ({} candidate(s) pending, all hosts)",
            staging.display(),
            staging::pending_count(&staging)
        );
        for note in ledger::read(&logs).unreadable {
            println!("warn  ledger stream unreadable: {note}");
        }
    }
    if let Some(note) = migrate::legacy_note(&state) {
        println!("warn  {note}");
    }

    match daemon::call("ping", std::time::Duration::from_millis(300)) {
        Some(r) => println!("ok    daemon answers (v{})", r.version.unwrap_or_default()),
        None => println!("warn  daemon not running — hooks fall back to direct reads"),
    }

    if let Some(issues) = install::codex_registration_issues() {
        if issues.is_empty() {
            println!("ok    Codex integration current (AGENTS.md + native hooks + MCP)");
            println!("note  Codex requires one-time hook approval in /hooks after changes");
        } else {
            for issue in issues {
                println!("FAIL  Codex integration: {issue}");
                hard_failures += 1;
            }
        }
    }

    if let Some(cfg) = &cfg {
        if cfg.serve.enabled {
            println!("ok    serving mode: origin {}", serve::origin_of(cfg));
            if let (Some(bind), Some(tf)) = (&cfg.serve.bind, &cfg.serve.token_file) {
                match serve::read_token(tf, true) {
                    Ok(_) => println!("ok    tcp serving configured on {bind} (token file is 0600)"),
                    Err(e) => {
                        println!("FAIL  tcp serving: {e}");
                        hard_failures += 1;
                    }
                }
            } else {
                println!("ok    serving on the local control channel only (no serve.bind)");
            }
        }
        if let Some(cs) = &cfg.client.serving {
            // A none-tier host without its serving host has NO memory at all
            // — that is a hard failure, not a warning.
            match serve::client_call(cs, serde_json::json!({"op": "generation"}), serve::QUERY_TIMEOUT) {
                Ok(r) => println!(
                    "ok    client mode (none-tier): serving host {} answers (origin {}, generation {})",
                    cs.addr,
                    r.origin.unwrap_or_default(),
                    r.generation.unwrap_or(0)
                ),
                Err(e) => {
                    println!("FAIL  client mode (none-tier): {e}");
                    hard_failures += 1;
                }
            }
        }
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

/// Commands that need a local index refuse on a none-tier host instead of
/// quietly building a parallel truth next to the remote routing.
fn none_tier_guard(cfg: &config::Config, what: &str) -> anyhow::Result<()> {
    if let Some(cs) = &cfg.client.serving {
        anyhow::bail!(
            "{what} needs a local index, but this host is none-tier by config \
             (client.serving = {}): queries are served remotely and no local index exists",
            cs.addr
        );
    }
    Ok(())
}

fn scan(background: bool) -> anyhow::Result<()> {
    let cfg = config::Config::load()?;
    none_tier_guard(&cfg, "scan")?;
    {
        let mut conn = index::open(&paths::state_dir())?;
        let native = paths::native_projects_root();
        let report = index::scan(&mut conn, &cfg.brain_root, Some(&native), &cfg.rings())?;
        println!(
            "indexed {} docs, {} blocks (generation {}, {} file(s) skipped as ring 5+)",
            report.docs, report.blocks, report.generation, report.skipped_high_ring
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
    // None-tier: the serving host's code index answers; no local index opens.
    if let Some(cs) = config::Config::load()?.client.serving {
        let body = serde_json::json!({"op": "find", "query": query, "limit": limit});
        let resp = serve::client_call(&cs, body, serve::QUERY_TIMEOUT)?;
        let hits = resp.code_hits.clone().unwrap_or_default();
        if json {
            let arr: Vec<_> = hits
                .iter()
                .map(|h| {
                    serde_json::json!({
                        "path": h.path, "name": h.name, "kind": h.kind,
                        "lines": [h.start_line, h.end_line], "tokens_estimated": h.token_estimate,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::json!({
                    "hits": arr, "origin": resp.origin, "generation": resp.generation,
                    "fresh": resp.fresh, "stale_note": resp.stale_note,
                })
            );
            return Ok(());
        }
        if hits.is_empty() {
            println!("no hits for \"{query}\" (served by {})", resp.origin.unwrap_or_default());
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
        print_served_by(&resp);
        return Ok(());
    }
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

/// Renders one repo map, whoever computed it. The serving host and the
/// none-tier client print the SAME lines — only the coherence footer differs.
/// `served_by` is the serving host's address when the map came over the wire:
/// an empty remote index is not something the local host can fix by scanning.
fn print_map(m: &serve::WireMap, focus: Option<&str>, served_by: Option<&str>) {
    if m.lines.is_empty() {
        match served_by {
            Some(addr) => println!(
                "the code index on serving host {addr} is empty — scanning happens there, on the storage host"
            ),
            None => println!("code index is empty — run `cfetch scan` first"),
        }
        return;
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
}

fn map_cmd(focus: Option<&str>, budget_tokens: u64) -> anyhow::Result<()> {
    let cfg = config::Config::load()?;
    // None-tier: the serving host's code index answers, exactly as for find.
    // (scan and embed-index stay local — they write.)
    if let Some(cs) = &cfg.client.serving {
        let body = serde_json::json!({
            "op": "map", "focus": focus, "budget_tokens": budget_tokens,
        });
        let resp = serve::client_call(cs, body, serve::QUERY_TIMEOUT)?;
        let m = resp
            .map
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("serving host {} returned no map", cs.addr))?;
        print_map(m, focus, Some(&cs.addr));
        print_served_by(&resp);
        return Ok(());
    }
    let conn = index::open(&paths::state_dir())?;
    let m = graph::map(&conn, &cfg.effective_code_roots(), focus, budget_tokens)?;
    print_map(&m.into(), focus, None);
    Ok(())
}

/// Coherence footer for answers that went through a serving daemon.
fn print_served_by(resp: &daemon::Response) {
    let origin = resp.origin.clone().unwrap_or_default();
    let generation = resp.generation.unwrap_or(0);
    if resp.fresh == Some(false) {
        println!(
            "\nserved by {origin} (generation {generation}) — STALE: {}",
            resp.stale_note.clone().unwrap_or_else(|| "barrier expired".to_string())
        );
    } else {
        println!("\nserved by {origin} (generation {generation}, fresh)");
    }
}

fn print_wire_hits(hits: &[serve::WireHit]) {
    for h in hits {
        println!("{} {}:{}-{} (ring {})", h.cite, h.path, h.start_line, h.end_line, h.ring);
        println!("    {}", h.snippet);
        if !h.mirrors.is_empty() {
            println!("    (also at: {})", h.mirrors.join(", "));
        }
    }
}

/// recall/expand answered by a serving daemon (remote TCP or the local unix
/// socket) — shared rendering for both.
fn recall_served(resp: &daemon::Response, id: Option<&str>, json: bool) -> anyhow::Result<()> {
    if let Some(cite) = id {
        let blocks = resp.blocks.clone().unwrap_or_default();
        if blocks.is_empty() {
            println!(
                "no block with citation {cite} (index may have moved on — content-addressed ids change when the entry changes)"
            );
            return Ok(());
        }
        for b in &blocks {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "cite": b.cite, "path": b.path, "ring": b.ring,
                        "lines": [b.start_line, b.end_line], "text": b.text,
                        "origin": resp.origin, "generation": resp.generation, "fresh": resp.fresh,
                    })
                );
            } else {
                println!("{} {}:{}-{} (ring {})\n{}\n", b.cite, b.path, b.start_line, b.end_line, b.ring, b.text);
            }
        }
        if !json {
            print_served_by(resp);
        }
        return Ok(());
    }
    let hits = resp.hits.clone().unwrap_or_default();
    // The serving host's degradation is the caller's to see: it ranked on our
    // behalf against endpoints we cannot reach, so if it fell back we would
    // otherwise never know.
    if let Some(note) = &resp.note {
        eprintln!("cfetch recall: {note}");
    }
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
        println!(
            "{}",
            serde_json::json!({
                "hits": arr, "origin": resp.origin, "generation": resp.generation,
                "fresh": resp.fresh, "stale_note": resp.stale_note, "note": resp.note,
            })
        );
    } else if hits.is_empty() {
        println!("no hits");
        print_served_by(resp);
    } else {
        print_wire_hits(&hits);
        print_served_by(resp);
        println!("expand a hit: cfetch recall --id <citation>");
    }
    Ok(())
}

/// none-tier: recall/expand answered by the remote serving host. Unreachable
/// is an explicit error naming the host — this host has no local index to
/// fall back to, and must never pretend otherwise.
#[allow(clippy::too_many_arguments)] // thin CLI adapter, mirrors the flag set
fn recall_remote(
    cs: &config::ClientServingConfig,
    query: &str,
    id: Option<&str>,
    expand: bool,
    semantic: bool,
    hybrid: bool,
    slice: Option<&str>,
    limit: usize,
    json: bool,
) -> anyhow::Result<()> {
    if expand {
        eprintln!("note: --expand (linked docs) is not yet available over remote serving — showing hits only");
    }
    let body = match id {
        Some(cite) => serde_json::json!({"op": "expand", "cite": cite}),
        None => {
            if query.trim().is_empty() {
                anyhow::bail!("empty query (pass search terms or --id <citation>)");
            }
            // The serving host ranks: it holds the vectors and it is the one
            // with an endpoint to embed the query against. A client that
            // holds nothing is exactly who this is for.
            serde_json::json!({
                "op": "recall", "query": query, "limit": limit,
                "semantic": semantic, "hybrid": hybrid, "slice": slice,
            })
        }
    };
    let resp = serve::client_call(cs, body, serve::QUERY_TIMEOUT)?;
    recall_served(&resp, id, json)
}

#[allow(clippy::too_many_arguments)] // thin CLI adapter, mirrors the flag set
fn recall(
    query: &str,
    id: Option<&str>,
    expand: bool,
    semantic: bool,
    hybrid: bool,
    slice: Option<&str>,
    limit: usize,
    json: bool,
) -> anyhow::Result<()> {
    let cfg = config::Config::load()?;
    if let Some(cs) = &cfg.client.serving {
        return recall_remote(cs, query, id, expand, semantic, hybrid, slice, limit, json);
    }
    // On a serving host, plain recall/expand go through the local daemon's
    // drain barrier when it answers. The direct path below is an equally
    // coherent fallback: `ensure_fresh` stat-fingerprints the tree on every
    // query — it just lacks the generation label and daemon batching.
    if id.is_none() && query.trim().is_empty() {
        anyhow::bail!("empty query (pass search terms or --id <citation>)");
    }
    if cfg.serve.enabled
        && !(semantic || hybrid || expand)
        && let Some(resp) = daemon::call_req(
            &match id {
                Some(cite) => serde_json::json!({"op": "expand", "cite": cite}),
                None => serde_json::json!({"op": "recall", "query": query, "limit": limit}),
            },
            std::time::Duration::from_secs(8),
        )
        && resp.ok
    {
        return recall_served(&resp, id, json);
    }
    let native = paths::native_projects_root();
    let conn = index::ensure_fresh(&paths::state_dir(), &cfg.brain_root, Some(&native), &cfg.rings())?;

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
    // Semantic/hybrid answers carry their own degradation note: partial or
    // absent vector coverage is reported, never hidden behind a result that
    // silently fell back to lexical ranking.
    // One ranking pipeline, shared with the serving daemon: the same query
    // against the same tree must rank the same way whoever answers it.
    let ranked = pipeline::ranked(&cfg, &conn, query, limit, semantic, hybrid, slice)?;
    let (hits, note) = (ranked.hits, ranked.note);
    if let Some(note) = &note {
        eprintln!("cfetch recall: {note}");
    }
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
        // The note rides in the JSON too: an agent parsing stdout must see
        // the degradation its human would have read on stderr.
        println!(
            "{}",
            serde_json::json!({"hits": arr, "linked": links, "note": note})
        );
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

/// Ring-5 staging review over the shared tree. Flagging happens in the Stop
/// hook's traps; this is the human side of the ladder. Candidates from EVERY
/// host are listed, because the staging directory is one directory.
fn staging_cmd(action: StagingAction) -> anyhow::Result<()> {
    let cfg = config::Config::load()?;
    let ex = exhaust::Exhaust::from_config(&cfg);
    let state_dir = paths::state_dir();
    // A legacy per-host exhaust.db is imported once, and said out loud.
    if let Some(r) = migrate::import_legacy_exhaust(&state_dir, &ex)? {
        println!(
            "imported {} event(s) and {} ring-5 candidate(s) from {} into the tree",
            r.events,
            r.staged,
            r.db.display()
        );
    }
    let dir = ex.staging_dir.clone();
    match action {
        StagingAction::List { json } => {
            let rows = staging::list(&dir);
            if json {
                let arr: Vec<_> = rows
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "id": c.id, "reason": c.reason, "kind": c.kind,
                            "session_id": c.session, "host": c.host, "ts": c.ts,
                            "payload": c.payload,
                        })
                    })
                    .collect();
                println!("{}", serde_json::json!(arr));
            } else if rows.is_empty() {
                println!("staging is empty — no flagged exhaust awaiting review");
                println!("  ({})", dir.display());
            } else {
                for c in &rows {
                    let session = c.session.get(..8).unwrap_or(&c.session);
                    println!(
                        "{}  {}  {}  {}  session {}  {}",
                        c.id, c.reason, c.kind, c.host, session, c.payload
                    );
                }
                println!("\ndistill a candidate, then: cfetch staging consume <id> | dismiss <id>");
            }
        }
        StagingAction::Consume { id } => {
            if staging::consume(&dir, &id)? {
                // The file is gone, so the stream is what remembers the
                // decision and keeps the trap from re-staging it.
                let _ = ex.record_decision(&id, "consume");
                println!("consumed {id}");
            } else {
                anyhow::bail!(
                    "no staged candidate {id} (never flagged, or already consumed/dismissed)"
                );
            }
        }
        StagingAction::Dismiss { id } => {
            if staging::dismiss(&dir, &id)? {
                let _ = ex.record_decision(&id, "dismiss");
                println!("dismissed {id} (kept in {}/dismissed)", dir.display());
            } else {
                anyhow::bail!(
                    "no staged candidate {id} (never flagged, or already consumed/dismissed)"
                );
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
    let ledger = ledger::load_from(&paths::logs_dir(&cfg.brain_root));
    let report = audit::build(&audit::AuditPaths::defaults(), &ledger, cfg.budget_chars, now);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", audit::render(&report));
    }
    Ok(())
}

fn status() -> anyhow::Result<()> {
    daemon::status()?;
    // Diagnostics must survive a broken config: the paths below fall back to
    // the defaults rather than making `status` the second casualty.
    let cfg = match config::Config::load() {
        Ok(cfg) => cfg,
        Err(e) => {
            println!("config: {e} — showing the default locations below");
            config::Config::default()
        }
    };
    let logs = paths::logs_dir(&cfg.brain_root);
    let loaded = ledger::read(&logs);
    for note in &loaded.unreadable {
        println!("ledger: UNREADABLE stream {note}");
    }
    let hosts: Vec<String> = loaded.hosts.iter().cloned().collect();
    let ledger = loaded.ledger;
    let sessions = ledger.sessions.len();
    let injected: u64 = ledger
        .sessions
        .values()
        .flat_map(|s| s.by_source.values())
        .map(|t| t.tokens_estimated)
        .sum();
    println!(
        "ledger: {sessions} session(s) from {} host(s){} in {}",
        hosts.len(),
        if hosts.is_empty() { String::new() } else { format!(" ({})", hosts.join(", ")) },
        logs.display()
    );
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
    // Ring 5/6 live in the tree, so their figures are the fleet's, not this
    // machine's.
    let ex = exhaust::Exhaust::from_config(&cfg);
    let ring56 = ex.stats();
    if ring56.staged_total > 0 {
        let reasons = ring56
            .staged_by_reason
            .iter()
            .map(|(r, n)| format!("{r}: {n}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "staging: {} ring-5 candidate(s) awaiting distillation [{reasons}] in {}",
            ring56.staged_total,
            ex.staging_dir.display()
        );
    } else {
        println!("staging: no ring-5 candidates awaiting distillation");
    }
    println!(
        "exhaust: {} of ring-6 stream in {}",
        jsonl::human_bytes(ring56.bytes),
        logs.display()
    );
    // Semantic coverage belongs in status, not only in a query's warning:
    // an operator must be able to SEE that the vectors are missing before a
    // ranking quietly falls back to lexical.
    if cfg.embeddings.enabled {
        match semantic_status(&cfg) {
            Ok(line) => println!("{line}"),
            Err(e) => println!("semantic: unavailable ({e})"),
        }
    }
    // Same reasoning for reranking: an operator must see that the second
    // stage is configured and reachable BEFORE a query quietly answers in
    // retrieval order.
    if cfg.rerank.enabled {
        match rerank::RerankClient::new(&cfg.rerank) {
            Ok(c) => println!(
                "rerank: {} over the top {} hit(s)",
                c.model(),
                c.candidates()
            ),
            Err(e) => println!("rerank: unavailable ({e})"),
        }
    }
    let state = paths::state_dir();
    // The identity is what a peer grants a slice TO, so an operator needs to
    // be able to read it off the machine it belongs to.
    match net::endpoint_id(&state) {
        Ok(id) => println!("identity: {id}"),
        Err(e) => println!("identity: unavailable ({e})"),
    }
    if let Some(note) = migrate::legacy_note(&state) {
        println!("{note}");
    }
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

/// Coverage of the configured embeddings spec, local cache and shared store
/// side by side. A none-tier host reports the shared store alone — it holds
/// no index to cover.
/// Mints an invite to a slice and records it as pending on this host.
///
/// The ticket printed here is the only readable copy of its secret: the
/// origin keeps a hash, so an invite that is lost is re-minted rather than
/// recovered.
fn invite(
    slice: &str,
    mode: &str,
    expires_in_hours: Option<u64>,
    json: bool,
) -> anyhow::Result<()> {
    let cfg = config::Config::load()?;
    let model = cfg.slice_model()?;
    // Granting access to a slice this host does not define would hand out a
    // name that resolves to nothing.
    anyhow::ensure!(
        model.names().any(|n| n == slice),
        "no slice named {slice:?} on this host (configured: {})",
        if model.is_empty() { "none".into() } else { model.names().collect::<Vec<_>>().join(", ") }
    );
    let mode: grant::Mode = mode.parse()?;
    let origin = net::endpoint_id(&paths::state_dir())?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let expires_at = expires_in_hours.map(|h| now + h * 3600);
    let ticket =
        grant::invite(&cfg.brain_root, &origin.to_string(), slice, mode, now, expires_at)?;
    let text = ticket.encode();
    if json {
        println!(
            "{}",
            serde_json::json!({
                "ticket": text, "slice": slice, "mode": mode.as_str(),
                "origin": origin.to_string(), "expires_at": expires_at,
            })
        );
    } else {
        println!("{text}");
        eprintln!(
            "cfetch invite: one-time {} invite to slice {slice:?}{}. \
             The secret is not stored — re-mint if it is lost.",
            mode.as_str(),
            match expires_in_hours {
                Some(h) => format!(", expiring in {h}h"),
                None => String::new(),
            }
        );
    }
    Ok(())
}

/// Shows the grants this host has made, per slice.
///
/// Pending invites are listed too: an unused invite is a key someone still
/// holds, and it should be as visible as a redeemed one.
fn grants(json: bool) -> anyhow::Result<()> {
    let cfg = config::Config::load()?;
    let model = cfg.slice_model()?;
    let mut rows = Vec::new();
    for slice in model.names() {
        for g in grant::read(&cfg.brain_root, slice)? {
            rows.push(serde_json::json!({
                "slice": g.slice,
                "mode": g.mode.as_str(),
                "peer": g.peer,
                "state": if g.pending() { "pending" } else { "redeemed" },
                "expires_at": g.expires_at,
            }));
        }
    }
    if json {
        println!("{}", serde_json::json!({"grants": rows}));
        return Ok(());
    }
    if rows.is_empty() {
        println!("no slice has been granted to anyone from this host");
        return Ok(());
    }
    for r in &rows {
        let peer = r["peer"].as_str().unwrap_or("(unused invite)");
        println!(
            "{}: {} -> {} [{}]",
            r["slice"].as_str().unwrap_or("?"),
            r["mode"].as_str().unwrap_or("?"),
            peer,
            r["state"].as_str().unwrap_or("?")
        );
    }
    Ok(())
}

/// Redeems an invite.
///
/// When the origin shares this host's tree — the ordinary case inside one
/// storage group — redemption is a local operation on the grants file and no
/// network is involved at all. The secret is what proves the invite came from
/// this tree; the origin id in the ticket is only needed for dialling one
/// that does not.
fn join(ticket: &str, json: bool) -> anyhow::Result<()> {
    let cfg = config::Config::load()?;
    let t = grant::Ticket::decode(ticket)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    anyhow::ensure!(!t.expired(now), "this invite has expired — ask the origin for a new one");

    let me = net::endpoint_id(&paths::state_dir())?.to_string();
    match grant::redeem(&cfg.brain_root, &t.slice, &t.secret, &me, now) {
        Ok(g) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "slice": g.slice, "mode": g.mode.as_str(),
                        "origin": t.origin, "peer": me, "shared_tree": true,
                    })
                );
            } else {
                println!(
                    "joined slice {:?} as {} (origin {}, redeemed on the shared tree)",
                    g.slice,
                    g.mode.as_str(),
                    &t.origin[..t.origin.len().min(12)]
                );
            }
            Ok(())
        }
        // Only ONE failure is ambiguous from the outside: a secret this tree
        // has never seen might belong to an origin that does not share the
        // tree. Every other refusal — already redeemed, expired — is known
        // precisely, and dressing it up with a maybe would mislead.
        Err(e) if e.to_string().contains("not known to this host") => anyhow::bail!(
            "this tree has no invite matching that ticket for slice {:?}. Either the invite \
             is from an origin that does NOT share this tree — redeeming across storage \
             groups needs the iroh transport, which is not wired yet — or the ticket is \
             not genuine.",
            t.slice
        ),
        Err(e) => Err(e),
    }
}

/// Lists the slices and what each one holds.
///
/// Counts are derived from the indexed paths at call time, so what this
/// prints is what a `--slice` query would actually match — a stored count
/// could disagree with the configuration.
fn slices(json: bool) -> anyhow::Result<()> {
    let cfg = config::Config::load()?;
    let model = cfg.slice_model()?;
    let conn = index::open_ro(&paths::state_dir())?;
    let docs = index::doc_block_counts(&conn)?;

    // Every configured slice appears even when empty: a slice that claims
    // nothing is a configuration mistake worth seeing, not an absence.
    let mut order: Vec<String> = model.names().map(str::to_string).collect();
    order.push(config::ROOT_SLICE.to_string());
    let mut counts: std::collections::HashMap<&str, (usize, usize)> =
        order.iter().map(|n| (n.as_str(), (0, 0))).collect();
    for (path, blocks) in &docs {
        let name = model.slice_for(path);
        let e = counts.entry(name).or_insert((0, 0));
        e.0 += 1;
        e.1 += blocks;
    }

    if json {
        let arr: Vec<_> = order
            .iter()
            .map(|n| {
                let (d, b) = counts.get(n.as_str()).copied().unwrap_or((0, 0));
                serde_json::json!({"slice": n, "docs": d, "blocks": b})
            })
            .collect();
        println!("{}", serde_json::json!({"slices": arr}));
        return Ok(());
    }
    if model.is_empty() {
        println!("no slices configured — the whole tree is one implicit slice");
    }
    for n in &order {
        let (d, b) = counts.get(n.as_str()).copied().unwrap_or((0, 0));
        let prefixes = match model.prefixes_of(n) {
            None => "the whole tree, minus anything a slice claims".to_string(),
            Some(p) => p.join(", "),
        };
        println!("{n}: {d} doc(s), {b} block(s) — {prefixes}");
    }
    Ok(())
}

fn semantic_status(cfg: &config::Config) -> anyhow::Result<String> {
    let spec = cfg.embeddings.spec();
    let shared = vectors::VectorStore::open(&cfg.brain_root, &spec)?.len();
    if cfg.client.serving.is_some() {
        return Ok(format!(
            "semantic: no local index (none-tier) — shared store holds {shared} artifact(s) for {} at {} dims",
            spec.model, spec.dim
        ));
    }
    let conn = index::open_ro(&paths::state_dir())?;
    let (embedded, total) = index::vector_coverage(&conn, &spec)?;
    Ok(embed::coverage_status_line(&spec, embedded, total, shared))
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
            // Other agents (Codex, Gemini) follow symmetrically: install
            // registers, --remove strips every trace.
            let agents = if remove { install::uninstall_agents() } else { install::install_agents() };
            if let Err(e) = agents {
                eprintln!("cfetch install (other agents): {e}");
                std::process::exit(1);
            }
        }
        Command::Invite { slice, mode, expires_in_hours, json } => {
            if let Err(e) = invite(&slice, &mode, expires_in_hours, json) {
                eprintln!("cfetch invite: {e:#}");
                std::process::exit(1);
            }
        }
        Command::Join { ticket, json } => {
            if let Err(e) = join(&ticket, json) {
                eprintln!("cfetch join: {e:#}");
                std::process::exit(1);
            }
        }
        Command::Grants { json } => {
            if let Err(e) = grants(json) {
                eprintln!("cfetch grants: {e:#}");
                std::process::exit(1);
            }
        }
        Command::Hardware { json } => {
            let found = hardware::detect();
            let variant = hardware::recommended_variant(&found);
            let sel = engine::select(&found);
            let selected = serde_json::json!({
                "device": sel.device.describe(),
                "backend": sel.backend.name(),
                "format": sel.backend.format(),
            });
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "variant": variant,
                        "os": hardware::os_token(),
                        "x86_64_level": hardware::x86_64_level(),
                        "compiled_backends": engine::compiled_backends()
                            .iter().map(|b| b.name()).collect::<Vec<_>>(),
                        "selected": selected,
                        "devices": found.iter().map(|f| serde_json::json!({
                            "device": f.device.describe(),
                            "token": f.device.token(),
                            "class": format!("{:?}", f.device.class()).to_lowercase(),
                            "evidence": f.evidence,
                            "usable": f.usable().is_ok(),
                            "unusable_reason": f.usable().err().map(|e| e.reason()),
                            "caveat": f.caveat(),
                        })).collect::<Vec<_>>(),
                    })
                );
            } else {
                println!("detected, best first (policy: NPU > GPU > CPU):");
                for f in &found {
                    let mark = if f.usable().is_ok() { " " } else { "!" };
                    println!("{mark} {:<26} {}", f.device.describe(), f.evidence);
                    if let Err(why) = f.usable() {
                        println!("    UNUSABLE: {}", why.reason());
                    }
                    if let Some(note) = f.caveat() {
                        println!("    note: {note}");
                    }
                }
                let engines: Vec<&str> =
                    engine::compiled_backends().iter().map(|b| b.name()).collect();
                println!("\nthis build contains: {}", engines.join(", "));
                println!(
                    "it will use:        {} via {} ({})",
                    sel.device.describe(),
                    sel.backend.name(),
                    sel.backend.format()
                );
                println!("recommended variant: {variant}");
            }
        }
        Command::Identity { json } => {
            match net::endpoint_id(&paths::state_dir()) {
                Ok(id) if json => println!("{}", serde_json::json!({"endpoint_id": id.to_string()})),
                Ok(id) => println!("{id}"),
                Err(e) => {
                    eprintln!("cfetch identity: {e:#}");
                    std::process::exit(1);
                }
            }
        }
        Command::Slices { json } => {
            if let Err(e) = slices(json) {
                eprintln!("cfetch slices: {e:#}");
                std::process::exit(1);
            }
        }
        Command::Staging { action } => {
            if let Err(e) = staging_cmd(action) {
                eprintln!("cfetch staging: {e}");
                std::process::exit(1);
            }
        }
        Command::Dashboard => {
            // No none-tier guard: the dashboard routes to the serving host
            // like the rest of the read path.
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
        Command::Recall { query, id, expand, semantic, hybrid, slice, limit, json } => {
            if let Err(e) = recall(
                &query.join(" "),
                id.as_deref(),
                expand,
                semantic,
                hybrid,
                slice.as_deref(),
                limit,
                json,
            ) {
                eprintln!("cfetch recall: {e}");
                std::process::exit(1);
            }
        }
        Command::EmbedIndex { batch } => {
            let guard = config::Config::load().and_then(|c| none_tier_guard(&c, "embed-index"));
            if let Err(e) = guard.and_then(|()| embed::embed_index_cmd(batch)) {
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
