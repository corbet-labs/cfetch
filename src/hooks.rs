//! Hook entrypoints. Contract: read stdin, act within the latency budget, emit
//! at most one JSON object, ALWAYS exit 0 — a hook failure must degrade to
//! silence, never to a broken session.

use std::path::Path;
use std::time::Duration;

use crate::config::Config;
use crate::hook_io::{Emit, HookEvent};
use crate::{daemon, exhaust, govern, heartbeat, ledger, paths, resident, session_state, transcript};

const DAEMON_BUDGET: Duration = Duration::from_millis(250);

/// Files above this line count get symbol-slice hints at pre-read.
const LARGE_FILE_LINES: usize = 400;
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
fn resident_with_deadline(cfg: &Config) -> String {
    let cfg = cfg.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(resident::build(&cfg).text);
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
    let (digest, max_sessions) = match &cfg {
        Ok(cfg) => {
            // Prefer the warm daemon; fall back to a bounded direct read —
            // session start works with no daemon at all.
            let digest = match daemon::call("resident", DAEMON_BUDGET) {
                Some(r) if r.ok => r.digest.unwrap_or_default(),
                _ => resident_with_deadline(cfg),
            };
            (digest, cfg.ledger_max_sessions)
        }
        Err(_) => (String::new(), 200),
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
    ledger::book(event.session(), "resident-digest", emitted, max_sessions);
    // The config failure still counts as a hook failure for the heartbeat.
    cfg.map(|_| ())
}

/// UserPromptSubmit: drains the reminder queue onto the prompt — the
/// zero-extra-turn delivery channel for everything queued at Stop and by the
/// cadence counter.
fn user_prompt(event: &HookEvent) -> anyhow::Result<()> {
    user_prompt_drain(&paths::state_dir(), event, ledger_max_sessions())
}

fn user_prompt_drain(
    state_dir: &Path,
    event: &HookEvent,
    max_sessions: usize,
) -> anyhow::Result<()> {
    // Reminders describe the primary session's own activity; a subagent
    // prompt must never receive them — nor consume them out of the queue.
    if event.is_subagent() {
        return Ok(());
    }
    let mut st = session_state::load(state_dir, event.session());
    let reminders = st.drain_reminders();
    if reminders.is_empty() {
        return Ok(());
    }
    session_state::store(state_dir, event.session(), &st);
    let mut emit = Emit::new("UserPromptSubmit");
    for r in reminders {
        emit.add_context(r);
    }
    // ONE JSON object regardless of how many reminders were queued.
    let emitted = emit.finish();
    ledger::book_in(state_dir, event.session(), "reminders", emitted, max_sessions);
    Ok(())
}

/// Stop-side reminder producers (wt/govern): QUEUE only, never emit — a
/// Stop-level injection forces a whole extra model turn. Runs after the
/// capture half so candidates the traps just flagged are already countable.
fn stop_govern(state_dir: &Path, cfg: &Config, event: &HookEvent) -> anyhow::Result<()> {
    if !cfg.governance.enabled || event.is_subagent() {
        return Ok(());
    }
    let mut st = session_state::load(state_dir, event.session());
    let mut dirty = govern::queue_status_nudge(&mut st, &cfg.brain_root);
    dirty |= govern::queue_staging_visibility(&mut st, &state_dir.join("exhaust.db"));
    if dirty {
        session_state::store(state_dir, event.session(), &st);
    }
    Ok(())
}

/// Cadence re-injection (wt/govern): every `reinject_every`-th post-tool
/// event queues the top ring-0 rules for the next user prompt. Queued here,
/// delivered at UserPromptSubmit — never at Stop.
fn post_tool_cadence(state_dir: &Path, cfg: &Config, event: &HookEvent) -> anyhow::Result<()> {
    if !cfg.governance.enabled || event.is_subagent() || event.tool_name.is_none() {
        return Ok(());
    }
    let mut st = session_state::load(state_dir, event.session());
    if let Some(n) = st.count_tool_event(cfg.governance.reinject_every)
        && let Some(rules) = govern::top_ring0_rules(cfg)
    {
        st.queue_reminder(&format!("rules-{n}"), &rules);
    }
    // The counter advanced even when nothing was queued.
    session_state::store(state_dir, event.session(), &st);
    Ok(())
}

/// Ring-6 exhaust capture (from wt/capture). The exhaust DB lives in the
/// LOCAL state dir (never NFS), so a direct short-timeout write stays inside
/// the hook latency budget without involving the daemon — and capture keeps
/// working when no daemon runs at all. Emits nothing.
fn post_tool_capture(
    state_dir: &std::path::Path,
    cfg: &Config,
    event: &HookEvent,
) -> anyhow::Result<()> {
    if !cfg.capture.enabled {
        return Ok(());
    }
    let conn = exhaust::open(state_dir)?;
    exhaust::capture_post_tool(&conn, event)
}

/// Turn summary + the 6->5 flagging traps (from wt/capture). Emits nothing —
/// Stop-level additionalContext would force an extra model turn, and ring
/// 5/6 content is never injected anyway.
fn stop_capture(state_dir: &std::path::Path, cfg: &Config, event: &HookEvent) -> anyhow::Result<()> {
    if !cfg.capture.enabled {
        return Ok(());
    }
    let conn = exhaust::open(state_dir)?;
    exhaust::record_stop(&conn, event.session())
}

fn ledger_max_sessions() -> usize {
    Config::load().map(|c| c.ledger_max_sessions).unwrap_or(200)
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

/// Number of lines in a file, byte-counted (no UTF-8 requirement). `None`
/// when unreadable.
fn line_count(path: &Path) -> Option<usize> {
    let bytes = std::fs::read(path).ok()?;
    let newlines = bytes.iter().filter(|b| **b == b'\n').count();
    Some(newlines + usize::from(!bytes.is_empty() && bytes.last() != Some(&b'\n')))
}

/// Read-only code-index query: named slices of `file_path`, largest first,
/// formatted `name start-end`. ANY failure (no DB yet, WAL sidecars
/// unreadable, schema drift) yields no hints — a missed optimization, never a
/// broken hook. The read-only open also guarantees the hook cannot contend
/// with the indexer for the write lock.
fn symbol_slices(db: &Path, file_path: &str, limit: usize) -> Vec<String> {
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

/// PreToolUse: read hygiene advice at the moment of spend. Repeat-read
/// advisory (once per file per session, mtime-gated, disarmed by compaction)
/// plus symbol-slice hints for large indexed files.
fn pre_tool(event: &HookEvent) -> anyhow::Result<()> {
    // Subagents run in fresh context: the "earlier content" a warning points
    // at does not exist there. Never warn subagents.
    if event.is_subagent() || event.tool_name.as_deref() != Some("Read") {
        return Ok(());
    }
    let Some(input) = event.tool_input.as_ref() else { return Ok(()) };
    let Some(path) = read_target(input) else { return Ok(()) };
    if is_ranged(input) {
        return Ok(());
    }

    let state_dir = paths::state_dir();
    let mut st = session_state::load(&state_dir, event.session());
    let mut emit = Emit::new("PreToolUse");
    let mut dirty = false;

    if let Some(mtime) = session_state::file_mtime(Path::new(path))
        && st.should_warn_repeat_read(path, mtime)
    {
        emit.add_context(format!(
            "[cfetch: {path} was already read this session (unchanged); prefer the earlier content or a narrower slice]"
        ));
        dirty = true;
    }

    if line_count(Path::new(path)).is_some_and(|n| n > LARGE_FILE_LINES) {
        let hints = symbol_slices(&state_dir.join("index.db"), path, MAX_SLICE_HINTS);
        if !hints.is_empty() && st.should_hint_slices(path) {
            emit.add_context(format!(
                "[cfetch: {path} is >{LARGE_FILE_LINES} lines; known symbol slices: {}]",
                hints.join(", ")
            ));
            dirty = true;
        }
    }

    let emitted = emit.finish();
    if dirty {
        session_state::store(&state_dir, event.session(), &st);
    }
    if emitted > 0 {
        // Book our own injection — advice that costs tokens is not free.
        ledger::book(event.session(), "read-advisory", emitted, ledger_max_sessions());
    }
    Ok(())
}

/// PostToolUse: ALL halves run — ring-6 exhaust capture (wt/capture), cadence
/// counting (wt/govern), and session read/write tracking (wt/account). A
/// failure in one half must not starve the others; the first error is still
/// reported to the heartbeat.
fn post_tool(event: &HookEvent) -> anyhow::Result<()> {
    let mut first_err: Option<anyhow::Error> = None;
    if event.tool_name.is_some() {
        let state_dir = paths::state_dir();
        match Config::load() {
            Ok(cfg) => {
                if let Err(e) = post_tool_capture(&state_dir, &cfg, event) {
                    first_err = Some(e);
                }
                if let Err(e) = post_tool_cadence(&state_dir, &cfg, event) {
                    first_err.get_or_insert(e);
                }
            }
            Err(e) => first_err = Some(e),
        }
    }
    if let Err(e) = post_tool_track(event) {
        first_err.get_or_insert(e);
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Session read/write tracking: full reads recorded with the file's mtime;
/// writes clear the read record (post-edit content is new content).
fn post_tool_track(event: &HookEvent) -> anyhow::Result<()> {
    if event.is_subagent() {
        return Ok(());
    }
    let (Some(tool), Some(input)) = (event.tool_name.as_deref(), event.tool_input.as_ref()) else {
        return Ok(());
    };
    let state_dir = paths::state_dir();
    match tool {
        "Read" => {
            let Some(path) = read_target(input) else { return Ok(()) };
            if is_ranged(input) {
                return Ok(());
            }
            let Some(mtime) = session_state::file_mtime(Path::new(path)) else { return Ok(()) };
            let mut st = session_state::load(&state_dir, event.session());
            st.record_read(path, mtime);
            session_state::store(&state_dir, event.session(), &st);
        }
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => {
            let target = input
                .get("file_path")
                .or_else(|| input.get("notebook_path"))
                .and_then(|v| v.as_str());
            let Some(path) = target else { return Ok(()) };
            let mut st = session_state::load(&state_dir, event.session());
            st.record_write(path);
            session_state::store(&state_dir, event.session(), &st);
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
    match Config::load() {
        Ok(cfg) => {
            if let Err(e) = stop_capture(&state_dir, &cfg, event) {
                first_err = Some(e);
            }
            if let Err(e) = stop_govern(&state_dir, &cfg, event) {
                first_err.get_or_insert(e);
            }
        }
        Err(e) => first_err = Some(e),
    }
    if let Err(e) = stop_measure(event) {
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
fn stop_measure(event: &HookEvent) -> anyhow::Result<()> {
    // A subagent's usage belongs to its own transcript, not this ledger row.
    if event.is_subagent() {
        return Ok(());
    }
    let Some(tp) = event.transcript_path.as_deref() else { return Ok(()) };
    let Some(usage) = transcript::scan(Path::new(tp)) else { return Ok(()) };
    ledger::book_measured(
        event.session(),
        &ledger::MeasuredUsage::from(&usage),
        ledger_max_sessions(),
    );
    Ok(())
}

/// PreCompact: after compaction the model no longer remembers its earlier
/// reads, so repeat-read warnings are disarmed for the rest of the session.
fn precompact(event: &HookEvent) -> anyhow::Result<()> {
    if event.is_subagent() {
        return Ok(());
    }
    let state_dir = paths::state_dir();
    let mut st = session_state::load(&state_dir, event.session());
    st.compacted = true;
    session_state::store(&state_dir, event.session(), &st);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CaptureConfig;
    use serde_json::json;

    fn bash_event(cmd: &str) -> HookEvent {
        HookEvent {
            session_id: Some("s1".into()),
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": cmd})),
            ..Default::default()
        }
    }

    #[test]
    fn capture_disabled_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg =
            Config { capture: CaptureConfig { enabled: false }, ..Config::default() };
        post_tool_capture(dir.path(), &cfg, &bash_event("ls")).unwrap();
        stop_capture(dir.path(), &cfg, &bash_event("ls")).unwrap();
        assert!(
            !dir.path().join("exhaust.db").exists(),
            "disabled capture must not even create the exhaust db"
        );
    }

    #[test]
    fn capture_enabled_records_the_event() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::default();
        post_tool_capture(dir.path(), &cfg, &bash_event("cargo build")).unwrap();
        let conn = exhaust::open(dir.path()).unwrap();
        let n: i64 = conn.query_row("SELECT count(*) FROM events", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn user_prompt_drains_queued_reminders_once_and_books_them() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = session_state::SessionState::default();
        assert!(st.queue_reminder("status", "update the STATUS"));
        assert!(st.queue_reminder("staging", "look at staging"));
        session_state::store(dir.path(), "s1", &st);

        let event = HookEvent { session_id: Some("s1".into()), ..Default::default() };
        user_prompt_drain(dir.path(), &event, 10).unwrap();

        let back = session_state::load(dir.path(), "s1");
        assert!(back.queued_reminders.is_empty(), "delivery must empty the queue");
        assert!(back.shown_keys.contains("status") && back.shown_keys.contains("staging"));
        let ledger = ledger::load_from(dir.path());
        let booked = &ledger.sessions["s1"].by_source["reminders"];
        assert_eq!(booked.count, 1, "many reminders, ONE emit, one booking");
        assert!(booked.chars > 0);

        // A second prompt delivers (and books) nothing further.
        user_prompt_drain(dir.path(), &event, 10).unwrap();
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
        user_prompt_drain(dir.path(), &sub, 10).unwrap();
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
            }],
            governance: crate::config::GovernanceConfig { enabled, reinject_every },
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
    fn line_count_counts_an_unterminated_last_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        std::fs::write(&p, "a\nb\nc").unwrap();
        assert_eq!(line_count(&p), Some(3));
        std::fs::write(&p, "a\nb\n").unwrap();
        assert_eq!(line_count(&p), Some(2));
        std::fs::write(&p, "").unwrap();
        assert_eq!(line_count(&p), Some(0));
        assert_eq!(line_count(Path::new("/nonexistent/cfetch-linecount")), None);
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
}
