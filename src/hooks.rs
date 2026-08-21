//! Hook entrypoints. Contract: read stdin, act within the latency budget, emit
//! at most one JSON object, ALWAYS exit 0 — a hook failure must degrade to
//! silence, never to a broken session.

use std::path::Path;
use std::time::Duration;

use crate::config::Config;
use crate::hook_io::{Emit, HookEvent};
use crate::{daemon, heartbeat, ledger, paths, resident, session_state, transcript};

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

/// PostToolUse: record what the session actually saw. Full reads are
/// recorded with the file's mtime; writes clear the read record (post-edit
/// content is new content).
fn post_tool(event: &HookEvent) -> anyhow::Result<()> {
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

/// Stop: book measured transcript usage for this turn. The transcript's
/// counters are cumulative, so the ledger books only the delta above its
/// watermark. An unparseable transcript books NOTHING — status then labels
/// the numbers estimated instead of inventing measured zeros.
fn stop(event: &HookEvent) -> anyhow::Result<()> {
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
