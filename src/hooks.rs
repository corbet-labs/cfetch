//! Hook entrypoints. Contract: read stdin, act within the latency budget, emit
//! at most one JSON object, ALWAYS exit 0 — a hook failure must degrade to
//! silence, never to a broken session.

use std::path::Path;
use std::time::Duration;

use crate::config::Config;
use crate::hook_io::{Emit, HookEvent};
use crate::resident::SessionScope;
use crate::{
    condense, daemon, exhaust, fsutil, govern, heartbeat, ledger, migrate, paths, resident,
    session_state, transcript,
};

const DAEMON_BUDGET: Duration = Duration::from_millis(250);
/// Private full-output artifacts are a recovery path, not an unbounded log.
const CONDENSED_OUTPUT_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Where this host books its ledger lines: the tree's log directory, this
/// host's identity, and the writer-side byte cap. Derived from the config,
/// with working defaults when the config is the thing that broke — booking is
/// a fact of record and must not stop because a setting is unreadable.
struct LedgerSink {
    dir: std::path::PathBuf,
    host: String,
    cap: u64,
}

impl LedgerSink {
    fn of(cfg: Option<&Config>) -> LedgerSink {
        let defaults = Config::default();
        LedgerSink {
            dir: paths::logs_dir(cfg.map_or(&defaults.brain_root, |c| &c.brain_root)),
            host: paths::host_id(),
            cap: cfg.map_or(defaults.ledger_max_bytes, |c| c.ledger_max_bytes),
        }
    }

    fn book(&self, session: &str, source: &str, chars: usize) {
        ledger::book_injection(&self.dir, &self.host, self.cap, session, source, chars);
    }

    fn book_measured(&self, session: &str, usage: &ledger::MeasuredUsage) {
        ledger::book_measured(&self.dir, &self.host, self.cap, session, usage);
    }
}

/// Files above this byte size get symbol-slice hints at pre-read. A
/// metadata-only gate: the hook path must never read file bodies (a full
/// read to count lines measured 292ms on a big file and is unbounded on
/// NFS). Exact line counts, if ever wanted, come from the index at scan
/// time — never from the hook path.
const LARGE_FILE_BYTES: u64 = 16 * 1024;
/// At most this many slice hints per advisory — an index dump is not a hint.
const MAX_SLICE_HINTS: usize = 5;

/// Dispatches a hook event by name. Never returns an error to the harness.
pub fn run(event_name: &str) {
    let event = HookEvent::from_stdin();
    let result = match event_name {
        "session-start" => session_start(&event),
        "user-prompt" => user_prompt(&event),
        "pre-tool" => pre_tool(&event),
        "post-tool" => post_tool(&event),
        "stop" => stop(&event),
        "precompact" => precompact(&event),
        other => Err(anyhow::anyhow!("unknown hook: {other}")),
    };
    match result {
        Ok(()) => heartbeat::record_ok(event_name),
        Err(e) => heartbeat::record_error(event_name, &e.to_string()),
    }
    // Exit 0 unconditionally — see module doc.
}

/// Direct-read fallback with a hard deadline: the tree may be NFS, and a hung
/// mount must not eat the whole hook timeout. The worker thread is detached on
/// overrun.
fn resident_with_deadline(cfg: &Config, scope: &SessionScope) -> String {
    let cfg = cfg.clone();
    let scope = scope.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(resident::build(&cfg, &scope).text);
    });
    rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default()
}

fn session_start(event: &HookEvent) -> anyhow::Result<()> {
    // Subagents inherit fresh context on purpose; the resident set is for the
    // primary session (a fork re-injecting rings would double-pay the budget).
    if event.is_subagent() {
        return Ok(());
    }

    let mut emit = Emit::new("SessionStart");

    // Everything below the digest is CONFIG-INDEPENDENT and must reach the
    // model even when the config is the thing that broke — otherwise the one
    // surface built to announce breakage is suppressed by the breakage.
    let cfg = Config::load();
    // Which entries this session is entitled to is decided from the session
    // itself — the machine and the directory the agent was started in.
    let scope = SessionScope::from_event(event);
    let digest = match &cfg {
        Ok(cfg) => {
            // Prefer the warm daemon; fall back to a bounded direct read —
            // session start works with no daemon at all. The daemon shares
            // the host but not the cwd, so the scope travels with the call.
            let req = serde_json::json!({ "op": "resident", "cwd": event.cwd });
            match daemon::call_req(&req, DAEMON_BUDGET) {
                Some(r) if r.ok => r.digest.unwrap_or_default(),
                _ => resident_with_deadline(cfg, &scope),
            }
        }
        Err(_) => String::new(),
    };

    let reason = event.start_reason();
    if !digest.is_empty() {
        if reason == "compact" || reason == "resume" {
            emit.add_context(format!(
                "[cfetch resident memory (rings 0-1), re-injected after {reason}]\n{digest}"
            ));
        } else {
            emit.add_context(format!("[cfetch resident memory (rings 0-1)]\n{digest}"));
        }
    }

    if let Err(e) = &cfg {
        emit.add_context(format!(
            "[cfetch degraded: config unusable ({e}) — memory injection disabled; run `cfetch selfcheck`]"
        ));
    }
    let degraded = heartbeat::degraded();
    if !degraded.is_empty() {
        let names: Vec<String> = degraded.iter().map(|(n, _)| n.clone()).collect();
        emit.add_context(format!(
            "[cfetch degraded: hook(s) {} failing repeatedly — memory capture may be incomplete; run `cfetch status`]",
            names.join(", ")
        ));
    }

    let emitted = emit.finish();
    LedgerSink::of(cfg.as_ref().ok()).book(event.session(), "resident-digest", emitted);
    // The config failure still counts as a hook failure for the heartbeat.
    cfg.map(|_| ())
}

/// UserPromptSubmit: drains the reminder queue onto the prompt — the
/// zero-extra-turn delivery channel for everything queued at Stop and by the
/// cadence counter.
fn user_prompt(event: &HookEvent) -> anyhow::Result<()> {
    let cfg = Config::load().ok();
    user_prompt_drain(&paths::state_dir(), &LedgerSink::of(cfg.as_ref()), event)
}

fn user_prompt_drain(
    state_dir: &Path,
    sink: &LedgerSink,
    event: &HookEvent,
) -> anyhow::Result<()> {
    // Reminders describe the primary session's own activity; a subagent
    // prompt must never receive them — nor consume them out of the queue.
    if event.is_subagent() {
        return Ok(());
    }
    let reminders = session_state::update(state_dir, event.session(), |st| st.drain_reminders())
        .unwrap_or_default();
    if reminders.is_empty() {
        return Ok(());
    }
    let mut emit = Emit::new("UserPromptSubmit");
    for r in reminders {
        emit.add_context(r);
    }
    // ONE JSON object regardless of how many reminders were queued.
    let emitted = emit.finish();
    sink.book(event.session(), "reminders", emitted);
    Ok(())
}

/// Stop-side reminder producers (wt/govern): QUEUE only, never emit — a
/// Stop-level injection forces a whole extra model turn. Runs after the
/// capture half so candidates the traps just flagged are already countable.
fn stop_govern(state_dir: &Path, cfg: &Config, event: &HookEvent) -> anyhow::Result<()> {
    if !cfg.governance.enabled || event.is_subagent() {
        return Ok(());
    }
    let _ = session_state::update(state_dir, event.session(), |st| {
        govern::queue_status_nudge(st, &cfg.brain_root);
        govern::queue_staging_visibility(st, &paths::staging_dir(&cfg.brain_root));
    });
    Ok(())
}

/// Cadence re-injection (wt/govern): every `reinject_every`-th post-tool
/// event queues the top ring-0 rules for the next user prompt. Queued here,
/// delivered at UserPromptSubmit — never at Stop.
fn post_tool_cadence(state_dir: &Path, cfg: &Config, event: &HookEvent) -> anyhow::Result<()> {
    if !cfg.governance.enabled || event.is_subagent() || event.tool_name.is_none() {
        return Ok(());
    }
    let rules = govern::top_ring0_rules(cfg, &SessionScope::from_event(event));
    // The refresh re-injects ring-0 rules, so it obeys the same per-entry
    // scope SessionStart did — a file this session never received must not
    // arrive by the back door 25 tool calls later.
    let _ = session_state::update(state_dir, event.session(), |st| {
        if let Some(n) = st.count_tool_event(cfg.governance.reinject_every)
            && let Some(rules) = rules
        {
            st.queue_reminder(&format!("rules-{n}"), &rules);
        }
    });
    Ok(())
}

/// Ring-6 exhaust capture (from wt/capture). One `O_APPEND` line into this
/// host's stream in the tree: no daemon, no fsync, no read — the whole write
/// path is a single short append, so it stays inside the hook latency budget
/// even when the tree is a network mount. Emits nothing.
fn post_tool_capture(cfg: &Config, event: &HookEvent) -> anyhow::Result<()> {
    if !cfg.capture.enabled {
        return Ok(());
    }
    exhaust::Exhaust::from_config(cfg).capture_post_tool(event, &cfg.brain_root, &cfg.rings())
}

fn prune_condensed_outputs(dir: &Path, keep: &Path, max_bytes: u64) -> anyhow::Result<()> {
    let mut files = Vec::new();
    let mut total = 0u64;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|ext| ext.to_str()) != Some("txt")
            || !entry.file_type()?.is_file()
        {
            continue;
        }
        let metadata = entry.metadata()?;
        total = total.saturating_add(metadata.len());
        files.push((
            metadata.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            entry.path(),
            metadata.len(),
        ));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    for (_, path, len) in files {
        if total <= max_bytes {
            break;
        }
        if path == keep {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => total = total.saturating_sub(len),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Condenses a completed Codex Bash result and preserves the full original in
/// private local state. Current Codex PostToolUse input is identifiable by the
/// required `turn_id` plus a string `tool_response`; Claude does not get the
/// `continue:false` replacement output because it interprets that universal
/// field as an instruction to stop the agent.
fn codex_condensed_output(state_dir: &Path, event: &HookEvent) -> anyhow::Result<Option<String>> {
    use sha2::Digest as _;

    if event.turn_id.is_none() || event.tool_name.as_deref() != Some("Bash") {
        return Ok(None);
    }
    let Some(command) = event
        .tool_input
        .as_ref()
        .and_then(|input| input.get("command"))
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(None);
    };
    let Some(output) = event.tool_response.as_ref().and_then(serde_json::Value::as_str) else {
        return Ok(None);
    };
    if u64::try_from(output.len()).unwrap_or(u64::MAX) > CONDENSED_OUTPUT_MAX_BYTES {
        return Ok(None);
    }
    let condensed = condense::condense(command, output);
    if !condensed.was_condensed() {
        return Ok(None);
    }

    let artifact_identity = format!(
            "{}\0{}\0{}\0{}",
            event.session(),
            event.turn_id.as_deref().unwrap_or_default(),
            event.tool_use_id.as_deref().unwrap_or_default(),
            output
    );
    let id = format!("{:x}", sha2::Sha256::digest(artifact_identity.as_bytes()));
    let path = state_dir.join("condensed-output").join(format!("{id}.txt"));
    let preserved = format!(
        "command: {}\nsession: {}\ntool_use_id: {}\n\n{}",
        exhaust::redact_secrets(command),
        event.session(),
        event.tool_use_id.as_deref().unwrap_or("unknown"),
        output
    );
    fsutil::atomic_write(&path, preserved)?;
    prune_condensed_outputs(
        path.parent().expect("condensed output has a parent"),
        &path,
        CONDENSED_OUTPUT_MAX_BYTES,
    )?;
    Ok(Some(format!(
        "{}\n\n[cfetch: full uncondensed output preserved at {}]",
        condensed.text,
        path.display()
    )))
}

/// Turn summary + the 6->5 flagging traps (from wt/capture). Emits nothing —
/// Stop-level additionalContext would force an extra model turn, and ring
/// 5/6 content is never injected anyway.
fn stop_capture(state_dir: &std::path::Path, cfg: &Config, event: &HookEvent) -> anyhow::Result<()> {
    if !cfg.capture.enabled {
        return Ok(());
    }
    let ex = exhaust::Exhaust::from_config(cfg);
    // A legacy per-host exhaust.db moves into the tree once, silently: a hook
    // emits nothing, so the CLI is where the note about it is printed.
    let imported = migrate::import_legacy_exhaust(state_dir, &ex);
    ex.record_stop(event.session())?;
    imported.map(|_| ())
}

/// The Read tool's target file, if this is a Read invocation we track.
fn read_target(input: &serde_json::Value) -> Option<&str> {
    input.get("file_path")?.as_str()
}

/// A ranged read (offset/limit) is already narrow: it triggers no advice and
/// is never recorded — a ranged read must not mark a later full read as
/// duplicate.
fn is_ranged(input: &serde_json::Value) -> bool {
    input.get("offset").is_some() || input.get("limit").is_some()
}

/// A file read visible through either Claude's structured Read tool or
/// Codex's shell tool. The shell subset is deliberately strict: only a single
/// `cat`, `head`, `tail`, or `sed` invocation with one final file operand is
/// Does this shell command write something? Deliberately the MIRROR of the
/// read parser's caution: that one refuses anything ambiguous because it names
/// a path, while this one only ever increments a counter, so a false negative
/// (an unrecognized write) costs a missed tally and a false positive would
/// invent activity that never happened. Recognized: an unquoted `>`/`>>`
/// redirect, and a leading program whose whole purpose is to modify the
/// filesystem. `sed`/`perl` count only with an in-place flag.
fn is_shell_write(command: &str) -> bool {
    if has_unquoted_redirect(command) {
        return true;
    }
    // Any segment of a pipeline or list may be the writer.
    command.split(['|', ';', '\n']).any(|segment| {
        let mut words = segment.split_whitespace().peekable();
        while let Some(w) = words.peek() {
            let w = *w;
            if w == "sudo" || w == "env" || w == "time" || w == "nohup" || w.contains('=') {
                words.next();
            } else {
                break;
            }
        }
        let Some(prog) = words.next() else { return false };
        let prog = std::path::Path::new(prog)
            .file_name()
            .and_then(|p| p.to_str())
            .unwrap_or(prog);
        match prog {
            "tee" | "cp" | "mv" | "rm" | "rmdir" | "mkdir" | "touch" | "install" | "dd"
            | "truncate" | "ln" | "chmod" | "chown" | "patch" | "rsync" | "shred" | "unlink" => true,
            // In-place only: a plain `sed`/`perl` filters to stdout.
            "sed" | "perl" => words.any(|w| w == "-i" || w.starts_with("-i.") || w == "--in-place"),
            _ => false,
        }
    })
}

/// `>` or `>>` outside single or double quotes. A quoted `>` is data — most
/// commonly a here-doc body or an echoed string — not a redirect.
fn has_unquoted_redirect(command: &str) -> bool {
    let (mut single, mut double, mut escaped) = (false, false, false);
    for c in command.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if !single => escaped = true,
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '>' if !single && !double => return true,
            _ => {}
        }
    }
    false
}

/// recognized, never a pipeline, redirect, substitution, or compound command.
fn read_invocation(event: &HookEvent) -> Option<(String, bool)> {
    let tool = event.tool_name.as_deref()?;
    let input = event.tool_input.as_ref()?;
    if tool == "Read" {
        return Some((resolve_event_path(event, read_target(input)?), is_ranged(input)));
    }
    if tool != "Bash" {
        return None;
    }
    let command = input.get("command")?.as_str()?;
    let words = exhaust::shell_words(command);
    if words.len() < 2
        || words.iter().any(|w| {
            w.contains([';', '|', '>', '<', '`']) || w.contains("&&") || w.contains("$(")
        })
    {
        return None;
    }
    let program = Path::new(&words[0]).file_name()?.to_str()?;
    let target = words.last()?;
    if target == "-" || target.starts_with('-') {
        return None;
    }
    let ranged = match program {
        "cat" => {
            if !words[1..words.len() - 1]
                .iter()
                .all(|w| w.starts_with('-'))
            {
                return None;
            }
            false
        }
        "head" | "tail" => {
            let middle = &words[1..words.len() - 1];
            let mut i = 0;
            while i < middle.len() {
                let option = &middle[i];
                if matches!(option.as_str(), "-n" | "-c" | "--lines" | "--bytes") {
                    i += 1;
                    if i >= middle.len() || !middle[i].chars().all(|c| c.is_ascii_digit()) {
                        return None;
                    }
                } else if !option.starts_with('-') {
                    return None;
                }
                i += 1;
            }
            true
        }
        "sed"
            if words.len() == 3
                || (words.len() == 4 && matches!(words[1].as_str(), "-n" | "-e")) =>
        {
            true
        }
        _ => return None,
    };
    Some((resolve_event_path(event, target), ranged))
}

fn resolve_event_path(event: &HookEvent, path: &str) -> String {
    let path = Path::new(path);
    if path.is_absolute() {
        return path.to_string_lossy().into_owned();
    }
    event
        .cwd
        .as_deref()
        .map(|cwd| Path::new(cwd).join(path).to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Metadata-only size gate for the slice-hint advisory. `false` when the
/// file is unstattable — a missed hint beats a broken hook.
fn is_large_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.len() > LARGE_FILE_BYTES)
}

/// Read-only code-index query: named slices of `file_path`, largest first,
/// formatted `name start-end`. ANY failure (no DB yet, WAL sidecars
/// unreadable, schema drift) yields no hints — a missed optimization, never a
/// broken hook. The read-only open also guarantees the hook cannot contend
/// with the indexer for the write lock. Also serves the daemon's `slices` op.
pub(crate) fn symbol_slices(db: &Path, file_path: &str, limit: usize) -> Vec<String> {
    use rusqlite::OpenFlags;
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let Ok(conn) = rusqlite::Connection::open_with_flags(db, flags) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT s.name, s.start_line, s.end_line
         FROM symbols s JOIN code_files f ON f.id = s.file_id
         WHERE f.path = ?1
         ORDER BY (s.end_line - s.start_line) DESC, s.start_line ASC
         LIMIT ?2",
    ) else {
        return Vec::new();
    };
    stmt.query_map(rusqlite::params![file_path, limit as i64], |r| {
        Ok(format!("{} {}-{}", r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
    })
    .map(|rows| rows.filter_map(Result::ok).collect())
    .unwrap_or_default()
}

/// Serving-host probe for slice hints: shorter read budget than a full
/// query — an advisory must not stall the interactive path.
fn remote_slices(
    cs: &crate::config::ClientServingConfig,
    path: &str,
    limit: usize,
) -> Vec<String> {
    let body = serde_json::json!({"op": "slices", "path": path, "limit": limit});
    crate::serve::client_call(cs, body, Duration::from_secs(2))
        .ok()
        .and_then(|r| r.slices)
        .unwrap_or_default()
}

/// PreToolUse: read hygiene advice at the moment of spend. Repeat-read
/// advisory (once per file per session, mtime-gated, disarmed by compaction)
/// plus symbol-slice hints for large indexed files.
fn pre_tool(event: &HookEvent) -> anyhow::Result<()> {
    // Subagents run in fresh context: the "earlier content" a warning points
    // at does not exist there. Never warn subagents.
    if event.is_subagent() {
        return Ok(());
    }
    let Some((path, ranged)) = read_invocation(event) else { return Ok(()) };
    if ranged {
        return Ok(());
    }

    let cfg = Config::load().ok();
    let state_dir = paths::state_dir();
    let mut emit = Emit::new("PreToolUse");
    let mtime = session_state::file_mtime(Path::new(&path));
    let hints = if is_large_file(Path::new(&path)) {
        match cfg.as_ref().and_then(|c| c.client.serving.as_ref()) {
            Some(cs) => remote_slices(cs, &path, MAX_SLICE_HINTS),
            None => symbol_slices(&state_dir.join("index.db"), &path, MAX_SLICE_HINTS),
        }
    } else {
        Vec::new()
    };
    let (warn, show_hints) = session_state::update(&state_dir, event.session(), |st| {
        let warn = mtime.is_some_and(|mtime| st.should_warn_repeat_read(&path, mtime));
        let show_hints = !hints.is_empty() && st.should_hint_slices(&path);
        (warn, show_hints)
    })
    .unwrap_or((false, false));

    if warn {
        emit.add_context(format!(
            "[cfetch: {path} was already read this session (unchanged); prefer the earlier content or a narrower slice]"
        ));
    }
    if show_hints {
        emit.add_context(format!(
            "[cfetch: {path} is >{}KB; known symbol slices: {}]",
            LARGE_FILE_BYTES / 1024,
            hints.join(", ")
        ));
    }

    let emitted = emit.finish();
    if emitted > 0 {
        // Book our own injection — advice that costs tokens is not free.
        LedgerSink::of(cfg.as_ref()).book(event.session(), "read-advisory", emitted);
    }
    Ok(())
}

/// PostToolUse: ALL halves run — ring-6 exhaust capture (wt/capture), cadence
/// counting (wt/govern), and session read/write tracking (wt/account). A
/// failure in one half must not starve the others; the first error is still
/// reported to the heartbeat.
fn post_tool(event: &HookEvent) -> anyhow::Result<()> {
    let mut first_err: Option<anyhow::Error> = None;
    let state_dir = paths::state_dir();
    let replacement = match codex_condensed_output(&state_dir, event) {
        Ok(replacement) => replacement,
        Err(e) => {
            first_err = Some(e);
            None
        }
    };
    if event.tool_name.is_some() {
        match Config::load() {
            Ok(cfg) => {
                if let Err(e) = post_tool_capture(&cfg, event) {
                    first_err = Some(e);
                }
                if let Err(e) = post_tool_cadence(&state_dir, &cfg, event) {
                    first_err.get_or_insert(e);
                }
                if let Err(e) = post_tool_budget(&state_dir, &cfg, event) {
                    first_err.get_or_insert(e);
                }
            }
            Err(e) => first_err = Some(e),
        }
    }
    if let Err(e) = post_tool_track(event) {
        first_err.get_or_insert(e);
    }
    if let Some(replacement) = replacement {
        let mut emit = Emit::new("PostToolUse");
        emit.replace_tool_output(replacement);
        let emitted = emit.finish();
        let cfg = Config::load().ok();
        LedgerSink::of(cfg.as_ref()).book(event.session(), "output-condensation", emitted);
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Session read/write tracking: full reads recorded with the file's mtime;
/// writes clear the read record (post-edit content is new content).
/// Paths this event wrote, across both shapes: Codex's `apply_patch` carries
/// them inside the patch body, the write tools name one directly.
fn written_targets(event: &HookEvent) -> Vec<String> {
    match event.tool_name.as_deref() {
        Some("apply_patch") => exhaust::written_paths(event),
        Some("Write" | "Edit" | "MultiEdit" | "NotebookEdit") => event
            .tool_input
            .as_ref()
            .and_then(|input| {
                input
                    .get("file_path")
                    .or_else(|| input.get("notebook_path"))
                    .and_then(|v| v.as_str())
            })
            .map(|p| vec![p.to_string()])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// A brain file that outgrew its token budget costs every session that loads
/// it, for as long as it stays that size — so the warning belongs at the write,
/// where the author is still holding the context to act on it. One line, once
/// per file per session, never a block.
fn post_tool_budget(state_dir: &Path, cfg: &Config, event: &HookEvent) -> anyhow::Result<()> {
    if event.is_subagent() {
        return Ok(());
    }
    let budget = cfg.governance.state_file_budget_tokens;
    if budget == 0 {
        return Ok(());
    }
    for path in written_targets(event) {
        // Only the brain's own files: cfetch governs what it will be asked to
        // load, not the size of the user's source tree.
        if !Path::new(&path).starts_with(&cfg.brain_root) {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        let tokens = crate::hook_io::estimate_tokens(usize::try_from(meta.len()).unwrap_or(usize::MAX));
        if tokens <= budget {
            continue;
        }
        let name = Path::new(&path)
            .strip_prefix(&cfg.brain_root)
            .unwrap_or(Path::new(&path))
            .display()
            .to_string();
        let _ = session_state::update(state_dir, event.session(), |st| {
            st.queue_reminder(
                &format!("budget:{path}"),
                &format!(
                    "[cfetch: {name} is ~{tokens} tokens, over the {budget}-token budget for one brain file — split it or distil it; every session that loads it pays this]"
                ),
            );
        });
    }
    Ok(())
}

fn post_tool_track(event: &HookEvent) -> anyhow::Result<()> {
    if event.is_subagent() {
        return Ok(());
    }
    let (Some(tool), Some(input)) = (event.tool_name.as_deref(), event.tool_input.as_ref()) else {
        return Ok(());
    };
    let state_dir = paths::state_dir();
    match tool {
        "Read" | "Bash" => {
            if tool == "Bash"
                && input
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(is_shell_write)
            {
                let _ = session_state::update(&state_dir, event.session(), |st| {
                    st.record_shell_write();
                });
            }
            let Some((path, ranged)) = read_invocation(event) else { return Ok(()) };
            if ranged {
                return Ok(());
            }
            let Some(mtime) = session_state::file_mtime(Path::new(&path)) else { return Ok(()) };
            let _ = session_state::update(&state_dir, event.session(), |st| {
                st.record_read(&path, mtime);
            });
        }
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" | "apply_patch" => {
            if tool == "apply_patch" {
                let paths = exhaust::written_paths(event);
                if paths.is_empty() {
                    return Ok(());
                }
                let _ = session_state::update(&state_dir, event.session(), |st| {
                    for path in paths {
                        st.record_write(&path);
                    }
                });
                return Ok(());
            }
            let target = input
                .get("file_path")
                .or_else(|| input.get("notebook_path"))
                .and_then(|v| v.as_str());
            let Some(path) = target else { return Ok(()) };
            let _ = session_state::update(&state_dir, event.session(), |st| {
                st.record_write(path);
            });
        }
        _ => {}
    }
    Ok(())
}

/// Stop: ALL halves — exhaust turn summary + traps (wt/capture), reminder
/// producers (wt/govern, after the traps so fresh flags count), and
/// measured-usage booking (wt/account).
fn stop(event: &HookEvent) -> anyhow::Result<()> {
    let mut first_err: Option<anyhow::Error> = None;
    let state_dir = paths::state_dir();
    let cfg = Config::load();
    match &cfg {
        Ok(cfg) => {
            if let Err(e) = stop_capture(&state_dir, cfg, event) {
                first_err = Some(e);
            }
            if let Err(e) = stop_govern(&state_dir, cfg, event) {
                first_err.get_or_insert(e);
            }
        }
        Err(e) => first_err = Some(anyhow::anyhow!("{e}")),
    }
    if let Err(e) = stop_measure(&LedgerSink::of(cfg.as_ref().ok()), event) {
        first_err.get_or_insert(e);
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Books measured transcript usage for this turn. The transcript's counters
/// are cumulative, so the ledger books only the delta above its watermark. An
/// unparseable transcript books NOTHING — status then labels the numbers
/// estimated instead of inventing measured zeros.
fn stop_measure(sink: &LedgerSink, event: &HookEvent) -> anyhow::Result<()> {
    // A subagent's usage belongs to its own transcript, not this ledger row.
    if event.is_subagent() {
        return Ok(());
    }
    let Some(tp) = event.transcript_path.as_deref() else { return Ok(()) };
    let Some(usage) = transcript::scan(Path::new(tp)) else { return Ok(()) };
    sink.book_measured(event.session(), &ledger::MeasuredUsage::from(&usage));
    Ok(())
}

/// PreCompact: after compaction the model no longer remembers its earlier
/// reads, so repeat-read warnings are disarmed for the rest of the session.
fn precompact(event: &HookEvent) -> anyhow::Result<()> {
    if event.is_subagent() {
        return Ok(());
    }
    let state_dir = paths::state_dir();
    let _ = session_state::update(&state_dir, event.session(), |st| {
        st.compacted = true;
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    fn budget_event(session: &str, path: &std::path::Path) -> HookEvent {
        serde_json::from_value(serde_json::json!({
            "session_id": session,
            "tool_name": "Write",
            "tool_input": {"file_path": path.to_string_lossy()},
        }))
        .unwrap()
    }

    #[test]
    fn an_oversized_brain_file_is_named_once_with_its_size_and_a_remedy() {
        let brain = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let big = brain.path().join("knowledge/huge.md");
        std::fs::create_dir_all(big.parent().unwrap()).unwrap();
        std::fs::write(&big, "x".repeat(40_000)).unwrap();
        let cfg = Config { brain_root: brain.path().to_path_buf(), ..Config::default() };
        let event = budget_event("s-budget", &big);

        post_tool_budget(state.path(), &cfg, &event).unwrap();
        // Drain the way UserPromptSubmit does — through `update`, so the
        // shown-key survives. Draining a loaded copy would throw it away and
        // the once-per-file guarantee would look broken when it is not.
        let mut texts = Vec::new();
        let _ = crate::session_state::update(state.path(), "s-budget", |st| {
            texts = st.drain_reminders();
        });
        assert_eq!(texts.len(), 1, "exactly one warning: {texts:?}");
        assert!(texts[0].contains("knowledge/huge.md"), "{}", texts[0]);
        assert!(texts[0].contains("tokens"), "the size must be stated: {}", texts[0]);
        assert!(texts[0].contains("split it or distil it"), "a remedy is required: {}", texts[0]);

        // Same file again in the same session must not warn twice.
        post_tool_budget(state.path(), &cfg, &event).unwrap();
        let mut again = Vec::new();
        let _ = crate::session_state::update(state.path(), "s-budget", |st| {
            again = st.drain_reminders();
        });
        assert!(again.is_empty(), "the warning fired twice for one file: {again:?}");
    }

    #[test]
    fn a_small_file_and_a_file_outside_the_brain_are_both_silent() {
        let brain = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let cfg = Config { brain_root: brain.path().to_path_buf(), ..Config::default() };

        let small = brain.path().join("small.md");
        std::fs::write(&small, "x".repeat(100)).unwrap();
        post_tool_budget(state.path(), &cfg, &budget_event("s-small", &small)).unwrap();
        assert!(crate::session_state::load(state.path(), "s-small").drain_reminders().is_empty());

        // cfetch governs what it will be asked to load, not the user's source.
        let outside = tempfile::tempdir().unwrap();
        let theirs = outside.path().join("vendor.rs");
        std::fs::write(&theirs, "x".repeat(40_000)).unwrap();
        post_tool_budget(state.path(), &cfg, &budget_event("s-out", &theirs)).unwrap();
        assert!(crate::session_state::load(state.path(), "s-out").drain_reminders().is_empty());
    }

    #[test]
    fn a_zero_budget_disables_the_check() {
        let brain = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let big = brain.path().join("huge.md");
        std::fs::write(&big, "x".repeat(40_000)).unwrap();
        let cfg = Config {
            brain_root: brain.path().to_path_buf(),
            governance: crate::config::GovernanceConfig {
                state_file_budget_tokens: 0,
                ..Default::default()
            },
            ..Config::default()
        };
        post_tool_budget(state.path(), &cfg, &budget_event("s-zero", &big)).unwrap();
        assert!(crate::session_state::load(state.path(), "s-zero").drain_reminders().is_empty());
    }

    #[test]
    fn shell_writes_are_recognized_without_naming_the_target() {
        // Redirects, in either form.
        assert!(is_shell_write("cat x > /tmp/out"));
        assert!(is_shell_write("printf hi >> log.txt"));
        // Programs whose purpose is to modify the filesystem, incl. in a pipe.
        assert!(is_shell_write("sudo cp a b"));
        assert!(is_shell_write("echo hi | tee file"));
        assert!(is_shell_write("mkdir -p /tmp/x"));
        assert!(is_shell_write("sed -i 's/a/b/' f"));
        // A quoted '>' is data, not a redirect.
        assert!(!is_shell_write("echo \'a > b\'"));
        assert!(!is_shell_write("grep \"a > b\" file"));
        // Read-only commands, and sed without an in-place flag.
        assert!(!is_shell_write("cat file"));
        assert!(!is_shell_write("sed 's/a/b/' f"));
        assert!(!is_shell_write("ls -la"));
    }

    use super::*;
    use crate::config::CaptureConfig;
    use serde_json::json;

    /// Books straight into a tempdir, standing in for the tree's logs dir.
    fn test_sink(dir: &Path) -> LedgerSink {
        LedgerSink { dir: dir.to_path_buf(), host: "test-host".into(), cap: 1 << 20 }
    }

    fn bash_event(cmd: &str) -> HookEvent {
        HookEvent {
            session_id: Some("s1".into()),
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": cmd})),
            ..Default::default()
        }
    }

    /// A config whose brain tree is a tempdir, so capture writes into a
    /// throwaway `logs/cfetch` instead of the operator's tree.
    fn tree_cfg(brain: &Path, capture: bool) -> Config {
        Config {
            brain_root: brain.to_path_buf(),
            capture: CaptureConfig { enabled: capture },
            ..Config::default()
        }
    }

    #[test]
    fn capture_disabled_writes_nothing() {
        let state = tempfile::tempdir().unwrap();
        let brain = tempfile::tempdir().unwrap();
        let cfg = tree_cfg(brain.path(), false);
        post_tool_capture(&cfg, &bash_event("ls")).unwrap();
        stop_capture(state.path(), &cfg, &bash_event("ls")).unwrap();
        assert!(
            !paths::logs_dir(brain.path()).exists(),
            "disabled capture must not even create the exhaust stream"
        );
    }

    #[test]
    fn capture_enabled_appends_one_line_to_the_tree() {
        let brain = tempfile::tempdir().unwrap();
        let cfg = tree_cfg(brain.path(), true);
        post_tool_capture(&cfg, &bash_event("cargo build")).unwrap();
        let records =
            crate::jsonl::read_all(&paths::logs_dir(brain.path()), exhaust::STREAM).records;
        assert_eq!(records.len(), 1, "one tool call, one appended line");
        assert_eq!(records[0].kind(), "bash");
        assert_eq!(records[0].value("payload").unwrap()["command"], "cargo build");
    }

    #[test]
    fn codex_listing_output_is_condensed_and_preserved_privately() {
        let state = tempfile::tempdir().unwrap();
        let output = (0..200)
            .map(|i| format!("line {i} with enough content to make condensation worthwhile"))
            .collect::<Vec<_>>()
            .join("\n");
        let event = HookEvent {
            session_id: Some("s1".into()),
            turn_id: Some("turn-1".into()),
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "API_TOKEN=not-a-real-secret rg needle ."})),
            tool_response: Some(json!(output)),
            tool_use_id: Some("call-1".into()),
            ..Default::default()
        };

        let feedback = codex_condensed_output(state.path(), &event)
            .unwrap()
            .expect("long listing should be condensed");
        assert!(feedback.contains("line 0 with enough content"));
        assert!(feedback.contains("line 199 with enough content"));
        assert!(!feedback.contains("line 100 with enough content"));
        assert!(feedback.contains("full uncondensed output preserved at"));

        let files = std::fs::read_dir(state.path().join("condensed-output"))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(files.len(), 1);
        let preserved = std::fs::read_to_string(files[0].path()).unwrap();
        assert!(preserved.contains("line 100 with enough content"));
        assert!(!preserved.contains("not-a-real-secret"), "the command header is redacted");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(files[0].metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn condensation_is_codex_only_and_never_rewrites_verification() {
        let state = tempfile::tempdir().unwrap();
        let output = (0..200).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");

        let mut event = bash_event("rg needle .");
        event.tool_response = Some(json!(output));
        assert!(
            codex_condensed_output(state.path(), &event).unwrap().is_none(),
            "an event without Codex's turn_id must not receive continue:false"
        );

        event.turn_id = Some("turn-1".into());
        event.tool_input = Some(json!({"command": "cargo test --all"}));
        assert!(
            codex_condensed_output(state.path(), &event).unwrap().is_none(),
            "test output is never rewritten"
        );
        assert!(!state.path().join("condensed-output").exists());
    }

    #[test]
    fn condensed_output_retention_keeps_the_current_pointer_and_caps_old_files() {
        let dir = tempfile::tempdir().unwrap();
        let old_a = dir.path().join("a.txt");
        let old_b = dir.path().join("b.txt");
        let keep = dir.path().join("keep.txt");
        std::fs::write(&old_a, "aaaaaaaaaa").unwrap();
        std::fs::write(&old_b, "bbbbbbbbbb").unwrap();
        std::fs::write(&keep, "kkkkkkkkkk").unwrap();

        prune_condensed_outputs(dir.path(), &keep, 15).unwrap();

        assert!(keep.exists(), "the path emitted to the model must survive pruning");
        let bytes: u64 = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum();
        assert!(bytes <= 15, "old artifacts must be removed until the cap holds");
    }

    #[test]
    fn stop_capture_stages_into_the_shared_tree() {
        let state = tempfile::tempdir().unwrap();
        let brain = tempfile::tempdir().unwrap();
        let cfg = tree_cfg(brain.path(), true);
        let hot = brain.path().join("knowledge/hot.md");
        for session in ["s1", "s2"] {
            let event = HookEvent {
                session_id: Some(session.into()),
                tool_name: Some("Write".into()),
                tool_input: Some(json!({"file_path": hot.to_string_lossy()})),
                ..Default::default()
            };
            post_tool_capture(&cfg, &event).unwrap();
        }
        stop_capture(state.path(), &cfg, &bash_event("done")).unwrap();
        let staged = crate::staging::list(&paths::staging_dir(brain.path()));
        assert_eq!(staged.len(), 1, "the hot-file trap staged a ring-5 candidate");
        assert_eq!(staged[0].reason, "hot-file");
    }

    #[test]
    fn user_prompt_drains_queued_reminders_once_and_books_them() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = session_state::SessionState::default();
        assert!(st.queue_reminder("status", "update the STATUS"));
        assert!(st.queue_reminder("staging", "look at staging"));
        session_state::store(dir.path(), "s1", &st);

        let event = HookEvent { session_id: Some("s1".into()), ..Default::default() };
        let sink = test_sink(dir.path());
        user_prompt_drain(dir.path(), &sink, &event).unwrap();

        let back = session_state::load(dir.path(), "s1");
        assert!(back.queued_reminders.is_empty(), "delivery must empty the queue");
        assert!(back.shown_keys.contains("status") && back.shown_keys.contains("staging"));
        let ledger = ledger::load_from(dir.path());
        let booked = &ledger.sessions["s1"].by_source["reminders"];
        assert_eq!(booked.count, 1, "many reminders, ONE emit, one booking");
        assert!(booked.chars > 0);

        // A second prompt delivers (and books) nothing further.
        user_prompt_drain(dir.path(), &sink, &event).unwrap();
        let ledger = ledger::load_from(dir.path());
        assert_eq!(ledger.sessions["s1"].by_source["reminders"].count, 1);
    }

    #[test]
    fn user_prompt_never_serves_subagents() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = session_state::SessionState::default();
        assert!(st.queue_reminder("status", "for the primary session"));
        session_state::store(dir.path(), "s1", &st);

        let sub = HookEvent {
            session_id: Some("s1".into()),
            agent_id: Some("a1".into()),
            ..Default::default()
        };
        user_prompt_drain(dir.path(), &test_sink(dir.path()), &sub).unwrap();
        let back = session_state::load(dir.path(), "s1");
        assert_eq!(back.queued_reminders.len(), 1, "a subagent must not consume the queue");
        assert!(ledger::load_from(dir.path()).sessions.is_empty(), "and books nothing");
    }

    /// Config with governance settings and a tempdir brain holding one ring-0
    /// resident rules file.
    fn govern_cfg(brain: &Path, enabled: bool, reinject_every: u32) -> Config {
        std::fs::write(
            brain.join("rules.md"),
            "---\ndescription: never force off VMs\n---\nbody\n",
        )
        .unwrap();
        Config {
            brain_root: brain.to_path_buf(),
            resident: vec![crate::config::ResidentEntry {
                path: std::path::PathBuf::from("rules.md"),
                ring: 0,
                scope: crate::config::Scope::default(),
                weight: None,
            }],
            governance: crate::config::GovernanceConfig { enabled, reinject_every, ..Default::default() },
            ..Config::default()
        }
    }

    fn brain_writes(state_dir: &Path, session: &str, brain: &Path, n: usize) {
        let mut st = session_state::load(state_dir, session);
        for i in 0..n {
            st.record_write(&format!("{}/knowledge/f{i}.md", brain.display()));
        }
        session_state::store(state_dir, session, &st);
    }

    #[test]
    fn stop_producers_queue_only_when_governance_is_enabled() {
        let state = tempfile::tempdir().unwrap();
        let brain = tempfile::tempdir().unwrap();
        brain_writes(state.path(), "s1", brain.path(), 3);
        let event = HookEvent { session_id: Some("s1".into()), ..Default::default() };

        let off = govern_cfg(brain.path(), false, 25);
        stop_govern(state.path(), &off, &event).unwrap();
        assert!(
            session_state::load(state.path(), "s1").queued_reminders.is_empty(),
            "disabled governance must queue nothing"
        );

        let on = govern_cfg(brain.path(), true, 25);
        stop_govern(state.path(), &on, &event).unwrap();
        let st = session_state::load(state.path(), "s1");
        assert_eq!(st.queued_reminders.len(), 1);
        assert_eq!(st.queued_reminders[0].0, "status");
    }

    #[test]
    fn stop_producers_exempt_subagents() {
        let state = tempfile::tempdir().unwrap();
        let brain = tempfile::tempdir().unwrap();
        brain_writes(state.path(), "s1", brain.path(), 3);
        let sub = HookEvent {
            session_id: Some("s1".into()),
            agent_type: Some("worker".into()),
            ..Default::default()
        };
        let cfg = govern_cfg(brain.path(), true, 25);
        stop_govern(state.path(), &cfg, &sub).unwrap();
        assert!(session_state::load(state.path(), "s1").queued_reminders.is_empty());
    }

    #[test]
    fn cadence_queues_rules_at_exactly_n_and_2n() {
        let state = tempfile::tempdir().unwrap();
        let brain = tempfile::tempdir().unwrap();
        let cfg = govern_cfg(brain.path(), true, 2);
        let event = bash_event("ls");

        for _ in 0..4 {
            post_tool_cadence(state.path(), &cfg, &event).unwrap();
        }
        let st = session_state::load(state.path(), "s1");
        assert_eq!(st.tool_events, 4);
        let keys: Vec<&str> = st.queued_reminders.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["rules-2", "rules-4"], "fires at exactly N and 2N");
        assert!(st.queued_reminders[0].1.starts_with("[cfetch rule refresh]"));
        assert!(st.queued_reminders[0].1.contains("never force off VMs"));
    }

    #[test]
    fn cadence_skips_subagents_disabled_governance_and_non_tool_events() {
        let state = tempfile::tempdir().unwrap();
        let brain = tempfile::tempdir().unwrap();

        let off = govern_cfg(brain.path(), false, 2);
        post_tool_cadence(state.path(), &off, &bash_event("ls")).unwrap();

        let on = govern_cfg(brain.path(), true, 2);
        let mut sub = bash_event("ls");
        sub.agent_id = Some("a1".into());
        post_tool_cadence(state.path(), &on, &sub).unwrap();

        let mut no_tool = bash_event("ls");
        no_tool.tool_name = None;
        post_tool_cadence(state.path(), &on, &no_tool).unwrap();

        let st = session_state::load(state.path(), "s1");
        assert_eq!(st.tool_events, 0, "none of these may advance the counter");
        assert!(st.queued_reminders.is_empty());
    }

    #[test]
    fn symbol_slices_reads_the_index_readonly_and_fails_silent() {
        // Missing DB: no hints, no error, no DB file created.
        let empty = tempfile::tempdir().unwrap();
        let missing = empty.path().join("index.db");
        assert!(symbol_slices(&missing, "/x.rs", 5).is_empty());
        assert!(!missing.exists(), "read-only probe must not create a DB");

        // Real index: hints come back largest-slice-first as `name start-end`.
        let code = tempfile::tempdir().unwrap();
        let f = code.path().join("big.rs");
        std::fs::write(&f, "fn one() {\n}\nfn two() {\n    let a = 1;\n    let b = 2;\n}\n")
            .unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        crate::code::scan_code(&mut conn, &[code.path().to_path_buf()]).unwrap();
        drop(conn);
        let path = f.to_string_lossy().to_string();
        let hints = symbol_slices(&state.path().join("index.db"), &path, 5);
        assert_eq!(hints, vec!["two 3-6".to_string(), "one 1-2".to_string()]);
        assert_eq!(symbol_slices(&state.path().join("index.db"), &path, 1).len(), 1);
        assert!(symbol_slices(&state.path().join("index.db"), "/absent.rs", 5).is_empty());
    }

    #[test]
    fn large_file_gate_is_size_only_and_fails_quiet() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        std::fs::write(&p, vec![b'x'; LARGE_FILE_BYTES as usize]).unwrap();
        assert!(!is_large_file(&p), "exactly the threshold is not large");
        std::fs::write(&p, vec![b'x'; LARGE_FILE_BYTES as usize + 1]).unwrap();
        assert!(is_large_file(&p));
        std::fs::write(&p, "small").unwrap();
        assert!(!is_large_file(&p));
        assert!(!is_large_file(Path::new("/nonexistent/cfetch-large-gate")));
    }

    #[test]
    fn ranged_reads_are_exempt_from_tracking() {
        let full: serde_json::Value = serde_json::json!({"file_path": "/a.rs"});
        let ranged: serde_json::Value =
            serde_json::json!({"file_path": "/a.rs", "offset": 10, "limit": 40});
        assert!(!is_ranged(&full));
        assert!(is_ranged(&ranged));
        assert_eq!(read_target(&full), Some("/a.rs"));
        assert_eq!(read_target(&serde_json::json!({"command": "ls"})), None);
    }

    #[test]
    fn codex_shell_reads_are_recognized_only_for_a_safe_subset() {
        let cwd = tempfile::tempdir().unwrap();
        let event = |command: &str| HookEvent {
            cwd: Some(cwd.path().to_string_lossy().into_owned()),
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": command})),
            ..Default::default()
        };
        assert_eq!(
            read_invocation(&event("cat 'file with spaces.rs'")),
            Some((cwd.path().join("file with spaces.rs").to_string_lossy().into_owned(), false))
        );
        assert_eq!(
            read_invocation(&event("head -n 20 src/main.rs")),
            Some((cwd.path().join("src/main.rs").to_string_lossy().into_owned(), true))
        );
        assert_eq!(
            read_invocation(&event("sed -n '10,30p' src/main.rs")),
            Some((cwd.path().join("src/main.rs").to_string_lossy().into_owned(), true))
        );
        for unsafe_command in [
            "cat a.rs b.rs",
            "cat a.rs | sed -n 1p",
            "cat a.rs > copy.rs",
            "cat $(secret-path)",
        ] {
            assert_eq!(read_invocation(&event(unsafe_command)), None, "{unsafe_command}");
        }
    }
}
