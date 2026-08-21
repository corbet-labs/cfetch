//! One-time import of the legacy per-host `exhaust.db` into the tree.
//!
//! Ring 6 and ring 5 used to live in a SQLite database in the local state dir.
//! That made them data one machine could see, which is exactly what the tree
//! exists to prevent. The import runs once per host: unconsumed flagged rows
//! become ring-5 staging FILES, every event becomes a line in this host's
//! exhaust stream (with its ORIGINAL timestamp), and then the database is
//! LEFT ALONE — operator data is never deleted by a migration. A marker in
//! the state dir keeps the import from running twice; the CLI says once that
//! the database can now be removed.

use std::path::{Path, PathBuf};

use rusqlite::OpenFlags;

use crate::exhaust::Exhaust;
use crate::staging::{self, Candidate};

/// Legacy database file, relative to the per-host state dir.
const LEGACY_DB: &str = "exhaust.db";
/// Marker recording that this host's import already ran.
const MARKER: &str = "exhaust-db-imported";

/// What one import moved into the tree.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub events: usize,
    pub staged: usize,
    pub db: PathBuf,
}

pub fn legacy_db(state_dir: &Path) -> PathBuf {
    state_dir.join(LEGACY_DB)
}

fn marker(state_dir: &Path) -> PathBuf {
    state_dir.join(MARKER)
}

/// Imports the legacy database if there is one and it has not been imported
/// yet. `Ok(None)` = nothing to do. Errors are the caller's to log; the hook
/// path treats them as it treats any capture failure (heartbeat, never a
/// broken session).
pub fn import_legacy_exhaust(state_dir: &Path, ex: &Exhaust) -> anyhow::Result<Option<Report>> {
    let db = legacy_db(state_dir);
    if !db.is_file() || marker(state_dir).exists() {
        return Ok(None);
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = rusqlite::Connection::open_with_flags(&db, flags)?;
    let mut stmt = conn.prepare(
        "SELECT id, session_id, ts, kind, payload, flag, coalesce(flag_reason, ''), consumed
           FROM events ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, i64>(7)?,
        ))
    })?;

    let mut report = Report { db: db.clone(), ..Default::default() };
    for row in rows {
        let (id, session, ts, kind, payload, flag, reason, consumed) = row?;
        let payload: serde_json::Value =
            serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null);
        // Original timestamps win over "now": the imported history keeps its
        // shape, so the traps and the audit window still see it correctly.
        ex.record_at(ts, &session, &kind, &payload)?;
        report.events += 1;
        if flag != 1 || consumed != 0 {
            continue;
        }
        let candidate = Candidate {
            id: staging::id_for(&reason, &legacy_key(&reason, id, &session, &payload)),
            reason: reason.clone(),
            session: session.clone(),
            host: ex.host.clone(),
            ts,
            kind: kind.clone(),
            payload,
        };
        if staging::write(&ex.staging_dir, &candidate)? {
            report.staged += 1;
        }
    }
    std::fs::write(
        marker(state_dir),
        format!(
            "imported {} event(s) and {} ring-5 candidate(s) from {} into the tree\n",
            report.events,
            report.staged,
            db.display()
        ),
    )?;
    Ok(Some(report))
}

/// Trap key of an imported candidate, so a pattern that is still live derives
/// the SAME id as a fresh flagging would and is not staged twice.
fn legacy_key(reason: &str, id: i64, session: &str, payload: &serde_json::Value) -> String {
    let field = |k: &str| payload.get(k).and_then(serde_json::Value::as_str).unwrap_or_default();
    match reason {
        "hot-file" => field("file_path").to_string(),
        "recurring-failure" => field("norm").to_string(),
        "fix-discovered" => format!("{session}\u{0}{}", field("norm")),
        // Warnings and anything a future trap invented: unique per row.
        _ => format!("legacy:{id}"),
    }
}

/// A note for the CLI when the legacy database is still on disk after its
/// import: it is dead weight now, and only a human may delete it.
pub fn legacy_note(state_dir: &Path) -> Option<String> {
    let db = legacy_db(state_dir);
    if db.is_file() && marker(state_dir).exists() {
        return Some(format!(
            "note: {} was imported into the tree and is no longer used — it can be removed",
            db.display()
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonl;

    /// A legacy exhaust database in the shape the SQLite implementation left
    /// behind: plain events plus flagged staging rows in all three states.
    fn legacy_fixture(state_dir: &Path) {
        let conn = rusqlite::Connection::open(legacy_db(state_dir)).unwrap();
        conn.execute_batch(
            "CREATE TABLE events(
               id INTEGER PRIMARY KEY,
               session_id TEXT NOT NULL,
               ts INTEGER NOT NULL,
               kind TEXT NOT NULL,
               payload TEXT NOT NULL,
               flag INTEGER NOT NULL DEFAULT 0,
               flag_reason TEXT,
               consumed INTEGER NOT NULL DEFAULT 0);
             INSERT INTO events(session_id, ts, kind, payload, flag, flag_reason, consumed)
             VALUES ('s1', 1000, 'bash', '{\"command\":\"ls\",\"norm\":\"ls\"}', 0, NULL, 0),
                    ('s1', 1001, 'write', '{\"file_path\":\"/b/knowledge/hot.md\",\"ring\":3}',
                     1, 'hot-file', 0),
                    ('s2', 1002, 'bash', '{\"norm\":\"cargo test\",\"failed\":true}',
                     1, 'recurring-failure', 0),
                    ('s2', 1003, 'bash', '{\"norm\":\"old\"}', 1, 'fix-discovered', 1),
                    ('s2', 1004, 'bash', '{\"norm\":\"gone\"}', 1, 'hot-file', 2);",
        )
        .unwrap();
    }

    fn exhaust_at(tree: &Path) -> Exhaust {
        Exhaust::new(
            tree.join("logs/cfetch"),
            tree.join("staging/cfetch"),
            "host-alpha".into(),
            1 << 20,
        )
    }

    #[test]
    fn legacy_rows_become_tree_files_and_the_database_survives() {
        let state = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        legacy_fixture(state.path());
        let ex = exhaust_at(tree.path());

        let report = import_legacy_exhaust(state.path(), &ex).unwrap().unwrap();
        assert_eq!(report.events, 5, "every event is carried over");
        assert_eq!(report.staged, 2, "only UNCONSUMED flagged rows are still candidates");

        // Ring 6: lines in this host's stream, with their original timestamps.
        let stream = jsonl::read_all(&ex.logs_dir, crate::exhaust::STREAM);
        assert_eq!(stream.records.len(), 5);
        assert_eq!(stream.records[0].ts, 1000, "imported history keeps its shape");
        assert_eq!(stream.records[0].kind(), "bash");
        assert_eq!(stream.records[0].str("session"), "s1");
        assert!(stream.records.iter().all(|r| r.host == "host-alpha"));

        // Ring 5: markdown candidates, keyed the way the traps key them.
        let staged = staging::list(&ex.staging_dir);
        assert_eq!(staged.len(), 2);
        let reasons: Vec<&str> = staged.iter().map(|c| c.reason.as_str()).collect();
        assert!(reasons.contains(&"hot-file") && reasons.contains(&"recurring-failure"));
        let hot = staged.iter().find(|c| c.reason == "hot-file").unwrap();
        assert_eq!(hot.id, staging::id_for("hot-file", "/b/knowledge/hot.md"));
        assert_eq!(hot.payload["ring"], 3);

        // The operator's database is untouched.
        assert!(legacy_db(state.path()).is_file(), "a migration never deletes operator data");
        assert!(legacy_note(state.path()).unwrap().contains("can be removed"));
    }

    #[test]
    fn the_import_runs_exactly_once() {
        let state = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        legacy_fixture(state.path());
        let ex = exhaust_at(tree.path());

        assert!(import_legacy_exhaust(state.path(), &ex).unwrap().is_some());
        assert!(
            import_legacy_exhaust(state.path(), &ex).unwrap().is_none(),
            "the marker stops a second import"
        );
        assert_eq!(
            jsonl::read_all(&ex.logs_dir, crate::exhaust::STREAM).records.len(),
            5,
            "no event is imported twice"
        );
        assert_eq!(staging::list(&ex.staging_dir).len(), 2);
    }

    #[test]
    fn no_legacy_database_is_a_no_op() {
        let state = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let ex = exhaust_at(tree.path());
        assert!(import_legacy_exhaust(state.path(), &ex).unwrap().is_none());
        assert!(legacy_note(state.path()).is_none());
        assert!(!ex.logs_dir.exists(), "nothing to import creates nothing");
        assert!(!marker(state.path()).exists());
    }
}
