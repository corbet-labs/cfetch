//! Ring-6 exhaust: raw capture of what the agent actually did, plus the 6->5
//! flagging traps and the ring-5 staging store.
//!
//! Unlike `index.db` (a disposable derived cache), `exhaust.db` is DATA — the
//! raw material ring 5 is distilled from. It is never dropped on schema
//! mismatch and cannot be rebuilt from anywhere. It stays per-host LOCAL
//! (state dir, never NFS) and never leaves the machine: rings 5-6 are
//! untrusted by definition, surfaced through `cfetch staging` only, never
//! injected into a session.
//!
//! Secrets are redacted at FIRST capture (commands) or withheld entirely
//! (secret-shaped paths) so every downstream consumer inherits the guard.
//! Retention is enforced in the write path itself — a cap only enforced by a
//! cleanup job is not a cap.
//!
//! The 6->5 crossing is automatic and cheap SQL, run from the Stop hook:
//! - fix-discovered: a command failed, then the same normalized command later
//!   succeeded in the same session — both events carry the story.
//! - recurring-failure: the same normalized command failed in >=2 sessions.
//! - hot-file: the same file written >=3 times in one session.
//!
//! Flag = promoted to staging (`flag=1`). Flagging is idempotent (an event is
//! never flagged twice; the first reason wins). `consumed` encodes the staging
//! outcome: 0 = awaiting review, 1 = consumed by distillation, 2 = dismissed.
//!
//! Bash payloads store the buglog-style normalized command (`norm`) at capture
//! time so the traps stay pure SQL over `json_extract`.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use rusqlite::Connection;

use crate::hook_io::HookEvent;

/// Bump on schema change. Exhaust is data: a mismatch is an error for a human
/// (or a future migration), never a delete-and-rebuild.
const SCHEMA_VERSION: i64 = 1;

/// Writer-side retention cap on total events.
pub const MAX_EVENTS: i64 = 20_000;

const REDACTED: &str = "<redacted>";
/// Stored instead of a secret-shaped file path — even the path leaks.
pub const WITHHELD: &str = "<secret path withheld>";

fn db_path(state_dir: &Path) -> PathBuf {
    state_dir.join("exhaust.db")
}

/// Opens (creating if needed) the exhaust DB. A schema-version mismatch on a
/// non-empty DB is an ERROR: this store is data, never disposable.
pub fn open(state_dir: &Path) -> anyhow::Result<Connection> {
    std::fs::create_dir_all(state_dir)?;
    let path = db_path(state_dir);
    let conn = Connection::open(&path)
        .with_context(|| format!("open exhaust db {}", path.display()))?;
    // Short timeout: this DB is written from the hook path, which must degrade
    // to a dropped event rather than stall the agent's tool loop.
    conn.busy_timeout(std::time::Duration::from_millis(200))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != SCHEMA_VERSION {
        let tables: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table'",
            [],
            |r| r.get(0),
        )?;
        if tables > 0 {
            anyhow::bail!(
                "exhaust db {} has schema v{version}, this binary expects v{SCHEMA_VERSION}; \
                 exhaust is data — migrate it, it will not be dropped",
                path.display()
            );
        }
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS events(
           id INTEGER PRIMARY KEY,
           session_id TEXT NOT NULL,
           ts INTEGER NOT NULL,
           kind TEXT NOT NULL,
           payload TEXT NOT NULL,
           flag INTEGER NOT NULL DEFAULT 0,
           flag_reason TEXT,
           consumed INTEGER NOT NULL DEFAULT 0
         );
         CREATE INDEX IF NOT EXISTS events_session ON events(session_id);
         CREATE INDEX IF NOT EXISTS events_staging ON events(flag, consumed);",
    )?;
    Ok(conn)
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Appends one event, enforcing retention in the same write path.
pub fn record(
    conn: &Connection,
    session: &str,
    kind: &str,
    payload: &serde_json::Value,
) -> anyhow::Result<()> {
    record_capped(conn, session, kind, payload, MAX_EVENTS)
}

fn record_capped(
    conn: &Connection,
    session: &str,
    kind: &str,
    payload: &serde_json::Value,
    cap: i64,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO events(session_id, ts, kind, payload) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![session, now(), kind, payload.to_string()],
    )?;
    let count: i64 = conn.query_row("SELECT count(*) FROM events", [], |r| r.get(0))?;
    if count > cap {
        conn.execute(
            "DELETE FROM events WHERE id IN (SELECT id FROM events ORDER BY id LIMIT ?1)",
            [count - cap],
        )?;
    }
    Ok(())
}

/// Redacts secret-shaped material from a shell command: values of
/// secret-named KEY=value pairs, arguments of secret-named flags, URL
/// userinfo, JWTs, and long hex/base64 runs.
pub fn redact_secrets(cmd: &str) -> String {
    let mut out = String::with_capacity(cmd.len() + 16);
    let mut redact_next = false;
    let mut rest = cmd;
    while !rest.is_empty() {
        let ws_end = rest.find(|c: char| !c.is_whitespace()).unwrap_or(rest.len());
        out.push_str(&rest[..ws_end]);
        rest = &rest[ws_end..];
        if rest.is_empty() {
            break;
        }
        let w_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let word = &rest[..w_end];
        rest = &rest[w_end..];
        if redact_next {
            out.push_str(REDACTED);
            redact_next = false;
        } else {
            out.push_str(&redact_word(word, &mut redact_next));
        }
    }
    out
}

/// Does this look like the NAME of a secret? Deliberately over-approximate:
/// redacting a harmless value is free, missing a secret is not.
fn keyish(word: &str) -> bool {
    let w = word.trim_start_matches('-').to_ascii_lowercase();
    ["token", "secret", "key", "pass", "pwd", "auth", "credential"].iter().any(|k| w.contains(k))
}

fn redact_word(word: &str, redact_next: &mut bool) -> String {
    // KEY=value with a secret-shaped key: the whole value goes.
    if let Some((key, value)) = word.split_once('=')
        && !key.is_empty()
        && !value.is_empty()
        && keyish(key)
    {
        return format!("{key}={REDACTED}");
    }
    // --password / --api-key style flags redact their next argument.
    if word.starts_with('-') && !word.contains('=') && keyish(word) {
        *redact_next = true;
        return word.to_string();
    }
    // scheme://user:pass@host — the userinfo goes as one unit.
    if let Some(scheme_end) = word.find("://") {
        let tail = &word[scheme_end + 3..];
        if let Some(at) = tail.find('@') {
            let userinfo = &tail[..at];
            if userinfo.contains(':') && !userinfo.contains('/') {
                return format!("{}{REDACTED}@{}", &word[..scheme_end + 3], &tail[at + 1..]);
            }
        }
    }
    // Token-shaped runs inside the word: JWTs, long hex, long base64. The
    // base64 charset deliberately excludes '/' (it would swallow paths), and
    // demands mixed case plus a digit so long kebab-case words survive.
    let w = redact_jwt_runs(word);
    let w = redact_char_runs(&w, 32, REDACTED, is_hex, any_run);
    redact_char_runs(&w, 32, REDACTED, is_base64ish, mixed_case_with_digit)
}

fn is_hex(c: char) -> bool {
    c.is_ascii_hexdigit()
}

fn is_base64ish(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '=' | '-' | '_')
}

fn is_digit(c: char) -> bool {
    c.is_ascii_digit()
}

fn any_run(_run: &str) -> bool {
    true
}

fn mixed_case_with_digit(run: &str) -> bool {
    run.chars().any(|c| c.is_ascii_digit())
        && run.chars().any(|c| c.is_ascii_uppercase())
        && run.chars().any(|c| c.is_ascii_lowercase())
}

/// Replaces maximal runs of `member` chars that are at least `min_len` bytes
/// long AND pass `qualifies`. Runs are ASCII by construction, so byte length
/// equals char count.
fn redact_char_runs(
    s: &str,
    min_len: usize,
    replacement: &str,
    member: fn(char) -> bool,
    qualifies: fn(&str) -> bool,
) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = String::new();
    for c in s.chars() {
        if member(c) {
            run.push(c);
        } else {
            flush_run(&mut out, &mut run, min_len, replacement, qualifies);
            out.push(c);
        }
    }
    flush_run(&mut out, &mut run, min_len, replacement, qualifies);
    out
}

fn flush_run(
    out: &mut String,
    run: &mut String,
    min_len: usize,
    replacement: &str,
    qualifies: fn(&str) -> bool,
) {
    if run.len() >= min_len && qualifies(run) {
        out.push_str(replacement);
    } else {
        out.push_str(run);
    }
    run.clear();
}

fn is_jwtish(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
}

/// Redacts `eyJ…`-led base64url runs of at least 20 chars (a bare JWT header
/// is exactly 20) anywhere in the word, shorter lookalikes stay.
fn redact_jwt_runs(s: &str) -> String {
    let Some(start) = s.find("eyJ") else { return s.to_string() };
    let tail = &s[start..];
    let run_len = tail.find(|c: char| !is_jwtish(c)).unwrap_or(tail.len());
    if run_len >= 20 {
        format!("{}{REDACTED}{}", &s[..start], redact_jwt_runs(&tail[run_len..]))
    } else {
        format!("{}{}", &s[..start + 3], redact_jwt_runs(&s[start + 3..]))
    }
}

/// Buglog-style command signature: lowercase, collapse whitespace, paths ->
/// `<path>`, long hex runs -> `<hex>`, digit runs -> `n`. Makes "the same
/// command" match across retries, sessions, and machines.
pub fn normalize_command(cmd: &str) -> String {
    let mut tokens = Vec::new();
    for word in cmd.split_whitespace() {
        let lower = word.to_ascii_lowercase();
        if lower.contains('/') {
            tokens.push("<path>".to_string());
            continue;
        }
        let no_hex = redact_char_runs(&lower, 8, "<hex>", is_hex, any_run);
        tokens.push(redact_char_runs(&no_hex, 1, "n", is_digit, any_run));
    }
    tokens.join(" ")
}

/// Secret-shaped path predicate (the recall index's idea, extended with
/// secret-store path components): such paths are withheld at capture.
pub fn secret_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if lower.split('/').any(|c| matches!(c, "secrets" | ".ssh" | ".gnupg" | ".aws")) {
        return true;
    }
    let base = lower.rsplit('/').next().unwrap_or(&lower);
    base.contains("secret")
        || base.contains("credential")
        || base.contains("password")
        || base.starts_with(".env")
        || base.ends_with(".pem")
        || base.ends_with(".key")
        || base.ends_with(".keystore")
        || base.ends_with(".p12")
        || base.ends_with(".pfx")
        || base.ends_with(".tfstate")
        || base == "id_rsa"
        || base == "id_ed25519"
}

/// PostToolUse capture: Bash -> 'bash' (redacted command + norm + error
/// hint), Write|Edit|MultiEdit -> 'write', Read -> 'read'. Anything else, or
/// missing fields, records nothing.
pub fn capture_post_tool(conn: &Connection, event: &HookEvent) -> anyhow::Result<()> {
    let session = event.session();
    let str_field = |key: &str| {
        event.tool_input.as_ref().and_then(|i| i.get(key)).and_then(serde_json::Value::as_str)
    };
    match event.tool_name.as_deref() {
        Some("Bash") => {
            let Some(cmd) = str_field("command") else { return Ok(()) };
            let redacted = redact_secrets(cmd);
            let norm = normalize_command(&redacted);
            record(
                conn,
                session,
                "bash",
                &serde_json::json!({
                    "command": redacted, "norm": norm, "failed": tool_failed(event),
                }),
            )
        }
        Some("Write" | "Edit" | "MultiEdit") => {
            let Some(path) = str_field("file_path") else { return Ok(()) };
            record(conn, session, "write", &file_payload(path))
        }
        Some("Read") => {
            let Some(path) = str_field("file_path") else { return Ok(()) };
            record(conn, session, "read", &file_payload(path))
        }
        _ => Ok(()),
    }
}

fn file_payload(path: &str) -> serde_json::Value {
    let stored = if secret_path(path) { WITHHELD } else { path };
    serde_json::json!({"file_path": stored})
}

/// Error hint, parsed leniently: harness versions disagree on where failure
/// lives (a `tool_error` field, response `stderr`, error/is_error keys).
fn tool_failed(event: &HookEvent) -> bool {
    if event.tool_error.as_ref().is_some_and(|v| !v.is_null()) {
        return true;
    }
    let Some(obj) = event.tool_response.as_ref().and_then(serde_json::Value::as_object) else {
        return false;
    };
    if obj.get("stderr").and_then(serde_json::Value::as_str).is_some_and(|s| !s.trim().is_empty())
    {
        return true;
    }
    if ["tool_error", "error"].iter().any(|k| obj.get(*k).is_some_and(|v| !v.is_null())) {
        return true;
    }
    ["is_error", "isError"]
        .iter()
        .any(|k| obj.get(*k).and_then(serde_json::Value::as_bool) == Some(true))
}

/// Stop: writes one 'turn' summary event (counts by kind this session), then
/// runs the 6->5 traps.
pub fn record_stop(conn: &Connection, session: &str) -> anyhow::Result<()> {
    let mut counts = serde_json::Map::new();
    let mut stmt = conn.prepare(
        "SELECT kind, count(*) FROM events
          WHERE session_id = ?1 AND kind IN ('bash', 'write', 'read')
          GROUP BY kind ORDER BY kind",
    )?;
    let per_kind = stmt.query_map([session], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    for row in per_kind {
        let (kind, n) = row?;
        counts.insert(kind, serde_json::Value::from(n));
    }
    record(conn, session, "turn", &serde_json::Value::Object(counts))?;
    run_traps(conn, session)
}

/// The 6->5 crossing. Each trap is one UPDATE guarded by `flag = 0`, so an
/// already-flagged event is never re-flagged (the first reason wins) and a
/// re-run is a no-op. Traps run in most-specific-first order.
fn run_traps(conn: &Connection, session: &str) -> anyhow::Result<()> {
    // (a) fix-discovered: a normalized command failed, then the same
    // normalized command succeeded LATER in the same session. Both the
    // failure and the fix carry the story, so both are staged.
    conn.execute(
        "UPDATE events SET flag = 1, flag_reason = 'fix-discovered'
          WHERE flag = 0 AND session_id = ?1 AND kind = 'bash'
            AND json_extract(payload, '$.norm') IN (
              SELECT json_extract(f.payload, '$.norm') FROM events f
               WHERE f.session_id = ?1 AND f.kind = 'bash'
                 AND json_extract(f.payload, '$.failed')
                 AND EXISTS (
                   SELECT 1 FROM events s
                    WHERE s.session_id = ?1 AND s.kind = 'bash'
                      AND NOT json_extract(s.payload, '$.failed')
                      AND json_extract(s.payload, '$.norm') =
                          json_extract(f.payload, '$.norm')
                      AND s.id > f.id))",
        [session],
    )?;
    // (b) recurring-failure: the same normalized command failed in >= 2
    // distinct sessions (any session — the pattern is cross-session by
    // definition, so this trap scans globally).
    conn.execute(
        "UPDATE events SET flag = 1, flag_reason = 'recurring-failure'
          WHERE flag = 0 AND kind = 'bash' AND json_extract(payload, '$.failed')
            AND json_extract(payload, '$.norm') IN (
              SELECT json_extract(payload, '$.norm') FROM events
               WHERE kind = 'bash' AND json_extract(payload, '$.failed')
               GROUP BY json_extract(payload, '$.norm')
              HAVING count(DISTINCT session_id) >= 2)",
        [],
    )?;
    // (c) hot-file: the same file written >= 3 times in this session. Only
    // the LATEST write is staged (one candidate per hot file, not three),
    // and a file that already produced a flag never fires again
    // (sum(flag) = 0). Withheld secret paths all share one placeholder and
    // must never aggregate into a fake hot file.
    conn.execute(
        "UPDATE events SET flag = 1, flag_reason = 'hot-file'
          WHERE flag = 0 AND session_id = ?1 AND kind = 'write'
            AND id IN (
              SELECT max(w.id) FROM events w
               WHERE w.session_id = ?1 AND w.kind = 'write'
                 AND json_extract(w.payload, '$.file_path') <> ?2
               GROUP BY json_extract(w.payload, '$.file_path')
              HAVING count(*) >= 3 AND sum(w.flag) = 0)",
        rusqlite::params![session, WITHHELD],
    )?;
    Ok(())
}

/// One ring-5 staging candidate (a flagged, unconsumed event).
pub struct Staged {
    pub id: i64,
    pub session_id: String,
    pub ts: i64,
    pub kind: String,
    pub payload: String,
    pub reason: String,
}

/// Flagged, unconsumed events, newest first.
pub fn staging_list(conn: &Connection) -> anyhow::Result<Vec<Staged>> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id, ts, kind, payload, coalesce(flag_reason, '')
           FROM events WHERE flag = 1 AND consumed = 0 ORDER BY id DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Staged {
            id: r.get(0)?,
            session_id: r.get(1)?,
            ts: r.get(2)?,
            kind: r.get(3)?,
            payload: r.get(4)?,
            reason: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Marks a staged candidate consumed (a distillation session took it).
/// Returns false when no such candidate is staged.
pub fn consume(conn: &Connection, id: i64) -> anyhow::Result<bool> {
    mark(conn, id, 1)
}

/// Drops a staged candidate without consuming it. Returns false when no such
/// candidate is staged. The row keeps `flag=1` so the traps stay idempotent.
pub fn dismiss(conn: &Connection, id: i64) -> anyhow::Result<bool> {
    mark(conn, id, 2)
}

fn mark(conn: &Connection, id: i64, state: i64) -> anyhow::Result<bool> {
    let n = conn.execute(
        "UPDATE events SET consumed = ?2 WHERE id = ?1 AND flag = 1 AND consumed = 0",
        rusqlite::params![id, state],
    )?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    type Row = (i64, String, String, String, i64, Option<String>, i64);

    fn open_tmp() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(dir.path()).unwrap();
        (dir, conn)
    }

    fn bash_event(session: &str, cmd: &str, failed: bool) -> HookEvent {
        HookEvent {
            session_id: Some(session.into()),
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": cmd})),
            tool_response: Some(json!({"stderr": if failed { "boom" } else { "" }})),
            ..Default::default()
        }
    }

    fn file_event(session: &str, tool: &str, path: &str) -> HookEvent {
        HookEvent {
            session_id: Some(session.into()),
            tool_name: Some(tool.into()),
            tool_input: Some(json!({"file_path": path})),
            ..Default::default()
        }
    }

    /// (id, session_id, kind, payload, flag, flag_reason, consumed), by id.
    fn rows(conn: &Connection) -> Vec<Row> {
        let mut stmt = conn
            .prepare(
                "SELECT id, session_id, kind, payload, flag, flag_reason, consumed
                 FROM events ORDER BY id",
            )
            .unwrap();
        let it = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })
            .unwrap();
        it.map(Result::unwrap).collect()
    }

    /// (id, flag_reason) of flagged events, by id.
    fn flagged(conn: &Connection) -> Vec<(i64, String)> {
        rows(conn)
            .into_iter()
            .filter(|r| r.4 == 1)
            .map(|r| (r.0, r.5.unwrap_or_default()))
            .collect()
    }

    #[test]
    fn redaction_covers_each_secret_shape() {
        let cases = [
            // KEY=value with secret-shaped key
            ("export API_TOKEN=abc123", "export API_TOKEN=<redacted>"),
            ("PASSWORD=x id", "PASSWORD=<redacted> id"),
            ("AUTH_HEADER=Basic123", "AUTH_HEADER=<redacted>"),
            // --flag=value and --flag value style
            ("run --password=hunter2", "run --password=<redacted>"),
            ("mysql --password hunter2 -h db", "mysql --password <redacted> -h db"),
            ("curl --api-key sk-live-abc", "curl --api-key <redacted>"),
            // URL userinfo
            (
                "git clone https://julian:hunter2@github.com/x/y.git",
                "git clone https://<redacted>@github.com/x/y.git",
            ),
            // JWT
            (
                "echo eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.abcDEF123",
                "echo <redacted>",
            ),
            // 32+ hex run
            (
                "verify deadbeefdeadbeefdeadbeefdeadbeef",
                "verify <redacted>",
            ),
            // 32+ mixed-case base64 run
            (
                "echo VGhpc0lzQVNlY3JldFZhbHVlMTIzNDU2Nzg5MA==",
                "echo <redacted>",
            ),
        ];
        for (input, want) in cases {
            assert_eq!(redact_secrets(input), want, "input: {input}");
        }
    }

    #[test]
    fn redaction_leaves_ordinary_commands_alone() {
        let cases = [
            "cargo build --release --target-dir /very/long/path/to/the/target/directory",
            "git commit -m updating-the-frobnicator-subsystem-for-release",
            "rg pattern src --type rust",
            "echo eyJshort",
            "curl https://example.com/deep/path/segment",
        ];
        for input in cases {
            assert_eq!(redact_secrets(input), input, "must stay untouched: {input}");
        }
    }

    #[test]
    fn normalization_matches_across_noise() {
        assert_eq!(normalize_command("CARGO  TEST --lib"), normalize_command("cargo test --lib"));
        assert_eq!(
            normalize_command("bash /tmp/a/b.sh 42"),
            normalize_command("bash /opt/c/d.sh 7")
        );
        assert_eq!(normalize_command("git checkout deadbeefcafe1234"), "git checkout <hex>");
        assert_eq!(normalize_command("retry attempt 12"), "retry attempt n");
    }

    #[test]
    fn secret_path_predicate() {
        for yes in [
            "/home/x/.env.production",
            "/home/x/agents/mind/secrets/koyeb.yml",
            "/etc/ssl/private/server.key",
            "/home/x/credentials.json",
            "/home/x/my-password-list.md",
            "cert.pem",
            "/home/x/.ssh/config",
            "/home/x/id_ed25519",
            "/srv/deploy/terraform.tfstate",
        ] {
            assert!(secret_path(yes), "must be withheld: {yes}");
        }
        for no in ["/home/x/src/main.rs", "/home/x/keyboard.md", "notes/envelope-budget.md"] {
            assert!(!secret_path(no), "must pass through: {no}");
        }
    }

    #[test]
    fn secret_paths_are_withheld_at_capture() {
        let (_d, conn) = open_tmp();
        capture_post_tool(&conn, &file_event("s1", "Write", "/home/x/.env.production")).unwrap();
        capture_post_tool(&conn, &file_event("s1", "Edit", "/home/x/mind/secrets/koyeb.yml"))
            .unwrap();
        capture_post_tool(&conn, &file_event("s1", "Read", "/home/x/server.pem")).unwrap();
        capture_post_tool(&conn, &file_event("s1", "Write", "/home/x/src/main.rs")).unwrap();
        let all = rows(&conn);
        assert_eq!(all.len(), 4);
        for r in &all[..3] {
            let p: serde_json::Value = serde_json::from_str(&r.3).unwrap();
            assert_eq!(p["file_path"], WITHHELD, "row {}", r.0);
        }
        let p: serde_json::Value = serde_json::from_str(&all[3].3).unwrap();
        assert_eq!(p["file_path"], "/home/x/src/main.rs");
    }

    #[test]
    fn bash_capture_is_redacted_session_keyed_and_error_hinted() {
        let (_d, conn) = open_tmp();
        capture_post_tool(&conn, &bash_event("s1", "export API_TOKEN=abc123", true)).unwrap();
        capture_post_tool(&conn, &bash_event("s2", "ls", false)).unwrap();
        let all = rows(&conn);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].1, "s1");
        assert_eq!(all[0].2, "bash");
        let p: serde_json::Value = serde_json::from_str(&all[0].3).unwrap();
        assert_eq!(p["command"], "export API_TOKEN=<redacted>");
        assert_eq!(p["failed"], true);
        assert_eq!(p["norm"], "export api_token=<redacted>");
        assert_eq!(all[1].1, "s2");
        let p2: serde_json::Value = serde_json::from_str(&all[1].3).unwrap();
        assert_eq!(p2["failed"], false);
    }

    #[test]
    fn tool_error_field_counts_as_failure() {
        let (_d, conn) = open_tmp();
        let mut ev = bash_event("s1", "ls", false);
        ev.tool_error = Some(json!("exit status 1"));
        capture_post_tool(&conn, &ev).unwrap();
        let p: serde_json::Value = serde_json::from_str(&rows(&conn)[0].3).unwrap();
        assert_eq!(p["failed"], true);
    }

    #[test]
    fn non_capture_tools_and_missing_fields_are_ignored() {
        let (_d, conn) = open_tmp();
        let mut glob = bash_event("s1", "x", false);
        glob.tool_name = Some("Glob".into());
        capture_post_tool(&conn, &glob).unwrap();
        let mut no_cmd = bash_event("s1", "x", false);
        no_cmd.tool_input = Some(json!({"description": "no command field"}));
        capture_post_tool(&conn, &no_cmd).unwrap();
        let mut no_path = file_event("s1", "Write", "x");
        no_path.tool_input = Some(json!({}));
        capture_post_tool(&conn, &no_path).unwrap();
        assert!(rows(&conn).is_empty());
        // The three capture kinds map correctly.
        capture_post_tool(&conn, &file_event("s1", "MultiEdit", "/a/b.rs")).unwrap();
        capture_post_tool(&conn, &file_event("s1", "Read", "/a/b.rs")).unwrap();
        let all = rows(&conn);
        assert_eq!(all[0].2, "write");
        assert_eq!(all[1].2, "read");
    }

    #[test]
    fn retention_cap_holds_at_the_writer() {
        let (_d, conn) = open_tmp();
        for i in 0..8 {
            record_capped(&conn, "s1", "bash", &json!({"command": i.to_string()}), 5).unwrap();
        }
        let all = rows(&conn);
        assert_eq!(all.len(), 5, "cap must hold in the write path");
        assert_eq!(all[0].0, 4, "the oldest events are the ones deleted");
    }

    #[test]
    fn stop_records_turn_summary_counts() {
        let (_d, conn) = open_tmp();
        capture_post_tool(&conn, &bash_event("s1", "cargo test", true)).unwrap();
        capture_post_tool(&conn, &bash_event("s1", "ls", false)).unwrap();
        capture_post_tool(&conn, &file_event("s1", "Write", "/a/b.rs")).unwrap();
        capture_post_tool(&conn, &file_event("s1", "Read", "/a/b.rs")).unwrap();
        capture_post_tool(&conn, &bash_event("s2", "other session", false)).unwrap();
        record_stop(&conn, "s1").unwrap();
        let all = rows(&conn);
        let turn = all.last().unwrap();
        assert_eq!(turn.2, "turn");
        assert_eq!(turn.1, "s1");
        let p: serde_json::Value = serde_json::from_str(&turn.3).unwrap();
        assert_eq!(p["bash"], 2, "s2 activity must not leak into s1's summary");
        assert_eq!(p["write"], 1);
        assert_eq!(p["read"], 1);
    }

    #[test]
    fn fix_discovered_trap_fires_once() {
        let (_d, conn) = open_tmp();
        capture_post_tool(&conn, &bash_event("s1", "cargo test --lib", true)).unwrap();
        capture_post_tool(&conn, &bash_event("s1", "echo unrelated", false)).unwrap();
        // Same normalized command (case + whitespace noise), now succeeding.
        capture_post_tool(&conn, &bash_event("s1", "CARGO   TEST --lib", false)).unwrap();
        record_stop(&conn, "s1").unwrap();
        let first = flagged(&conn);
        assert_eq!(first.len(), 2, "both the failure and the fix are the story");
        assert!(first.iter().all(|(_, r)| r == "fix-discovered"));
        record_stop(&conn, "s1").unwrap();
        assert_eq!(flagged(&conn), first, "the trap must be idempotent");
    }

    #[test]
    fn no_fix_flag_when_success_precedes_failure() {
        let (_d, conn) = open_tmp();
        capture_post_tool(&conn, &bash_event("s1", "cargo test", false)).unwrap();
        capture_post_tool(&conn, &bash_event("s1", "cargo test", true)).unwrap();
        record_stop(&conn, "s1").unwrap();
        assert!(flagged(&conn).is_empty(), "success BEFORE failure is not a discovered fix");
    }

    #[test]
    fn recurring_failure_trap_fires_once() {
        let (_d, conn) = open_tmp();
        // Same normalized command (paths normalize away) failing in 2 sessions.
        capture_post_tool(&conn, &bash_event("s1", "bash /tmp/deploy.sh", true)).unwrap();
        capture_post_tool(&conn, &bash_event("s2", "bash /opt/deploy.sh", true)).unwrap();
        record_stop(&conn, "s2").unwrap();
        let first = flagged(&conn);
        assert_eq!(first.len(), 2);
        assert!(first.iter().all(|(_, r)| r == "recurring-failure"));
        record_stop(&conn, "s2").unwrap();
        record_stop(&conn, "s1").unwrap();
        assert_eq!(flagged(&conn), first, "the trap must be idempotent");
    }

    #[test]
    fn hot_file_trap_fires_once_and_skips_withheld() {
        let (_d, conn) = open_tmp();
        for _ in 0..3 {
            capture_post_tool(&conn, &file_event("s1", "Write", "/a/hot.rs")).unwrap();
            capture_post_tool(&conn, &file_event("s1", "Write", "/a/.env")).unwrap();
        }
        capture_post_tool(&conn, &file_event("s1", "Write", "/a/cold.rs")).unwrap();
        capture_post_tool(&conn, &file_event("s1", "Write", "/a/cold.rs")).unwrap();
        record_stop(&conn, "s1").unwrap();
        let f = flagged(&conn);
        assert_eq!(f.len(), 1, "one flag per hot file; withheld paths never aggregate");
        assert_eq!(f[0].1, "hot-file");
        let all = rows(&conn);
        let hit = all.iter().find(|r| r.0 == f[0].0).unwrap();
        let p: serde_json::Value = serde_json::from_str(&hit.3).unwrap();
        assert_eq!(p["file_path"], "/a/hot.rs");
        record_stop(&conn, "s1").unwrap();
        assert_eq!(flagged(&conn).len(), 1, "the trap must be idempotent");
    }

    #[test]
    fn consume_and_dismiss_leave_staging() {
        let (_d, conn) = open_tmp();
        for _ in 0..3 {
            capture_post_tool(&conn, &file_event("s1", "Write", "/a/one.rs")).unwrap();
            capture_post_tool(&conn, &file_event("s2", "Write", "/b/two.rs")).unwrap();
        }
        record_stop(&conn, "s1").unwrap();
        record_stop(&conn, "s2").unwrap();
        let staged = staging_list(&conn).unwrap();
        assert_eq!(staged.len(), 2);
        let (id1, id2) = (staged[1].id, staged[0].id);

        assert!(consume(&conn, id1).unwrap());
        assert!(!consume(&conn, id1).unwrap(), "consuming twice must report nothing to do");
        assert!(dismiss(&conn, id2).unwrap());
        assert!(!dismiss(&conn, id2).unwrap());
        assert!(!consume(&conn, id2).unwrap(), "dismissed candidates are gone from staging");
        assert!(staging_list(&conn).unwrap().is_empty());

        // Consumed and dismissed stay distinguishable, and both keep flag=1 so
        // the traps stay idempotent.
        let all = rows(&conn);
        let r1 = all.iter().find(|r| r.0 == id1).unwrap();
        let r2 = all.iter().find(|r| r.0 == id2).unwrap();
        assert_eq!((r1.4, r1.6), (1, 1));
        assert_eq!((r2.4, r2.6), (1, 2));
        record_stop(&conn, "s1").unwrap();
        record_stop(&conn, "s2").unwrap();
        assert!(staging_list(&conn).unwrap().is_empty(), "consumed/dismissed never re-stage");
    }

    #[test]
    fn staging_lists_newest_first() {
        let (_d, conn) = open_tmp();
        for _ in 0..3 {
            capture_post_tool(&conn, &file_event("s1", "Write", "/a/one.rs")).unwrap();
        }
        record_stop(&conn, "s1").unwrap();
        for _ in 0..3 {
            capture_post_tool(&conn, &file_event("s1", "Write", "/a/two.rs")).unwrap();
        }
        record_stop(&conn, "s1").unwrap();
        let staged = staging_list(&conn).unwrap();
        assert_eq!(staged.len(), 2);
        assert!(staged[0].id > staged[1].id, "newest first");
        assert_eq!(staged[0].reason, "hot-file");
        assert_eq!(staged[0].session_id, "s1");
        assert!(staged[0].payload.contains("/a/two.rs"));
    }

    #[test]
    fn schema_mismatch_preserves_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("exhaust.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 99i64).unwrap();
            conn.execute_batch("CREATE TABLE keepsake(x)").unwrap();
        }
        assert!(open(dir.path()).is_err(), "mismatch must be an error, not a rebuild");
        let conn = Connection::open(&path).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'keepsake'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "exhaust is data; nothing may delete it");
    }
}
