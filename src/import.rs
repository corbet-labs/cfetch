//! One-way importer from openwolf-enhanced `.wolf/` directories into the
//! cfetch brain tree. Preserves every markdown file an operator or agent
//! wrote; skips everything cfetch regenerates (code index, token ledger,
//! exhaust, embeddings, hooks, cron state); lists everything it does not
//! recognise instead of dropping it silently.
//!
//! The mapping is deliberate:
//!
//! | openwolf | cfetch | ring | why |
//! |---|---|---|---|
//! | `OPENWOLF.md` | `AGENT.md` | 1 | resident context, read every session |
//! | `cerebrum.md` | `knowledge/cerebrum.md` | 3 | curated, recallable |
//! | `memory.md` | `mind/memories/MEMORY.md` | 2 | behaviour, scoped injection |
//! | `identity.md` | `knowledge/identity.md` | 3 | project identity |
//! | `reframe-frameworks.md` | `knowledge/reframe-frameworks.md` | 3 | reference |
//! | `buglog.json` | `knowledge/bugs/<id>.md` | 3 | one note per curated bug; the ids stay greppable so `bug-NNN` references inside imported files keep resolving |
//! | archive `*.md` | `knowledge/archive/` | excl | out of the index |
//! | `todo/staging/*` | `todo/staging/*` | 5 | quarantine, same contract |
//!
//! Files cfetch derives on its own are skipped with a note: `anatomy.md`
//! (code index), `token-ledger.json`, `STATUS.md`, `config.json` (cfetch has
//! its own two-layer config), `recall-embeddings.*` (cfetch will re-embed),
//! hooks, logs, cron state. Anything else found at the top level is reported
//! as `unrecognized` and left in place — ring placement for somebody else's
//! conventions is the operator's call, not the importer's.
//!
//! The dry run and the real run share one code path: `collect()` builds the
//! same report either way, and only the real run writes. The two cannot
//! disagree with each other.

use std::path::Path;

pub struct ImportReport {
    pub imported: Vec<(String, String)>,
    pub skipped: Vec<(String, String)>,
    /// Top-level entries that matched no table — left in place, but named.
    pub unrecognized: Vec<String>,
    pub errors: Vec<(String, String)>,
}

/// Files that become brain content, with their destination and a ring
/// frontmatter to prepend (None = no frontmatter needed, the location
/// default already applies).
pub const MIGRATIONS: &[(&str, &str, Option<u8>)] = &[
    ("OPENWOLF.md", "AGENT.md", Some(1)),
    ("cerebrum.md", "knowledge/cerebrum.md", Some(3)),
    ("memory.md", "mind/memories/MEMORY.md", Some(2)),
    ("identity.md", "knowledge/identity.md", Some(3)),
    ("reframe-frameworks.md", "knowledge/reframe-frameworks.md", Some(3)),
];

/// Files cfetch regenerates or tracks differently — skipped with a reason.
const SKIPPED: &[(&str, &str)] = &[
    ("anatomy.md", "cfetch builds its own code index"),
    ("anatomy-graph.json", "derived from the code index"),
    ("anatomy-symbols.json", "derived from the code index"),
    ("token-ledger.json", "cfetch has its own token accounting"),
    ("STATUS.md", "runtime state, cfetch generates it"),
    ("config.json", "cfetch has its own two-layer config"),
    ("cron-manifest.json", "cfetch has its own cron engine"),
    ("cron-state.json", "cfetch has its own cron engine"),
    ("designqc-report.json", "cfetch generates its own"),
    ("suggestions.json", "cfetch generates its own"),
    ("recall-embeddings.json", "cfetch will re-embed with its model"),
    ("recall-embeddings.vec", "cfetch will re-embed with its model"),
];

/// Patterns for archive files — old content kept but excluded from the index
/// by cfetch's default `knowledge/archive/` exclude prefix.
const ARCHIVE_SUFFIXES: &[&str] = &[
    ".vor-aufraeumen",
    "-archiv-",
    "-backup-",
    "-old-",
];

fn is_archive(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    // The suffixes are specific enough (German "vor-aufräumen", dated
    // archives, backups) that they cannot false-positive on ordinary
    // brain files — no .md extension check needed, since backup naming
    // appends AFTER the extension (memory.md.vor-aufraeumen).
    ARCHIVE_SUFFIXES.iter().any(|s| lower.contains(s))
}

/// Prepends a `ring:` frontmatter block if the file doesn't already have one.
fn with_ring(raw: &str, ring: Option<u8>) -> String {
    let Some(ring) = ring else { return raw.to_string() };
    if raw.starts_with("---") {
        // Already has frontmatter; don't duplicate the fence.
        return raw.to_string();
    }
    format!("---\nring: {}\n---\n\n{}", ring, raw)
}

/// The openwolf-enhanced bug log: one hand-curated entry per failure, each
/// written down right after losing an afternoon to it. The schema was read
/// from exported `buglog.json` data files, never from openwolf source code.
#[derive(serde::Deserialize, Default)]
struct Buglog {
    #[serde(default)]
    bugs: Vec<BuglogEntry>,
}

#[derive(serde::Deserialize, Default)]
struct BuglogEntry {
    #[serde(default)]
    id: String,
    #[serde(default)]
    timestamp: String,
    #[serde(default)]
    error_message: String,
    #[serde(default)]
    file: String,
    #[serde(default)]
    root_cause: String,
    #[serde(default)]
    fix: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    related_bugs: Vec<String>,
    /// openwolf-enhanced writes this as a number; hand-edited logs have
    /// been seen with strings. Accept either.
    #[serde(default)]
    occurrences: serde_json::Value,
    #[serde(default)]
    last_seen: String,
}

impl BuglogEntry {
    fn occurrences_text(&self) -> String {
        match &self.occurrences {
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(s) => s.clone(),
            _ => String::new(),
        }
    }
}

/// One bug entry becomes one ring-3 note. The `bug-NNN` id is kept in the
/// filename and the title so references inside imported files stay greppable.
fn bug_note(entry: &BuglogEntry) -> String {
    let mut note = String::new();
    note.push_str("---\nring: 3\n---\n\n");
    note.push_str(&format!("# {} — {}\n\n", entry.id, entry.error_message));
    note.push_str(&format!("- **error**: {}\n", entry.error_message));
    if !entry.file.is_empty() {
        note.push_str(&format!("- **file**: {}\n", entry.file));
    }
    if !entry.timestamp.is_empty() {
        note.push_str(&format!("- **date**: {}", entry.timestamp));
        if !entry.last_seen.is_empty() {
            note.push_str(&format!(" (last seen {})", entry.last_seen));
        }
        note.push('\n');
    }
    if !entry.occurrences_text().is_empty() {
        note.push_str(&format!("- **occurrences**: {}\n", entry.occurrences_text()));
    }
    if !entry.tags.is_empty() {
        note.push_str(&format!("- **tags**: {}\n", entry.tags.join(", ")));
    }
    if !entry.related_bugs.is_empty() {
        note.push_str(&format!("\nrelated: {}\n", entry.related_bugs.join(", ")));
    }
    if !entry.root_cause.is_empty() {
        note.push_str("\n## Root cause\n\n");
        note.push_str(&entry.root_cause);
        note.push_str("\n\n");
    }
    if !entry.fix.is_empty() {
        note.push_str("## Fix\n\n");
        note.push_str(&entry.fix);
        note.push('\n');
    }
    note
}

/// Builds the complete import report. `execute == false` is the dry run:
/// identical report, nothing written. This is the ONE code path both runs
/// share, so the preview cannot disagree with the act.
fn collect(wolf_dir: &Path, brain_root: &Path, execute: bool) -> anyhow::Result<ImportReport> {
    anyhow::ensure!(
        wolf_dir.is_dir(),
        "{} is not a directory; point at the .wolf/ directory",
        wolf_dir.display()
    );
    let mut report = ImportReport {
        imported: Vec::new(),
        skipped: Vec::new(),
        unrecognized: Vec::new(),
        errors: Vec::new(),
    };

    // Migrate the named content files.
    for (src_name, dest_rel, ring) in MIGRATIONS {
        let src = wolf_dir.join(src_name);
        if !src.is_file() {
            continue;
        }
        let dest = brain_root.join(dest_rel);
        if dest.exists() {
            report.skipped.push((src_name.to_string(), "destination already exists".to_string()));
            continue;
        }
        if execute {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| anyhow::anyhow!("create {}: {e}", parent.display()))?;
            }
            let raw = std::fs::read_to_string(&src)
                .map_err(|e| anyhow::anyhow!("read {}: {e}", src.display()))?;
            std::fs::write(&dest, with_ring(&raw, *ring))
                .map_err(|e| anyhow::anyhow!("write {}: {e}", dest.display()))?;
        }
        report.imported.push((src_name.to_string(), dest_rel.to_string()));
    }

    // Migrate the curated bug log: one note per entry under knowledge/bugs/.
    // The ids stay in the filenames so `bug-NNN` references inside imported
    // files keep resolving; a broken log is an error entry, not an abort.
    let buglog = wolf_dir.join("buglog.json");
    if buglog.is_file() {
        let parsed = std::fs::read_to_string(&buglog)
            .map_err(|e| anyhow::anyhow!("read {}: {e}", buglog.display()))
            .and_then(|raw| serde_json::from_str::<Buglog>(&raw).map_err(|e| anyhow::anyhow!("parse buglog.json: {e}")));
        match parsed {
            Ok(log) if !log.bugs.is_empty() => {
                let mut written = 0usize;
                for (index, entry) in log.bugs.iter().enumerate() {
                    let id = if entry.id.is_empty() {
                        format!("bug-{}", index + 1)
                    } else {
                        entry.id.clone()
                    };
                    let dest = brain_root.join("knowledge").join("bugs").join(format!("{id}.md"));
                    if dest.exists() {
                        report.skipped.push((format!("buglog.json:{id}"), "already exists".to_string()));
                        continue;
                    }
                    if execute {
                        std::fs::create_dir_all(dest.parent().expect("bugs has a parent"))
                            .map_err(|e| anyhow::anyhow!("create knowledge/bugs: {e}"))?;
                        std::fs::write(&dest, bug_note(entry))
                            .map_err(|e| anyhow::anyhow!("write {}: {e}", dest.display()))?;
                    }
                    written += 1;
                }
                report.imported.push((
                    "buglog.json".to_string(),
                    format!("knowledge/bugs/ ({written} entries)"),
                ));
            }
            Ok(_) => {
                report.skipped.push(("buglog.json".to_string(), "no entries".to_string()));
            }
            Err(error) => {
                report.errors.push(("buglog.json".to_string(), error.to_string()));
            }
        }
    }

    // Migrate archive markdown files (handoff archives, re-notes, backups).
    for entry in std::fs::read_dir(wolf_dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if is_archive(&name) {
            let src = entry.path();
            let dest = brain_root.join("knowledge/archive").join(&name);
            if dest.exists() {
                report.skipped.push((name, "archive destination already exists".to_string()));
                continue;
            }
            if execute {
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                if let Err(e) = std::fs::copy(&src, &dest) {
                    report.errors.push((name.clone(), e.to_string()));
                    continue;
                }
            }
            report.imported.push((name.clone(), format!("knowledge/archive/{}", name)));
        }
    }

    // Migrate staging candidates (quarantine in cfetch has the same contract).
    let staging_src = wolf_dir.join("todo").join("staging");
    if staging_src.is_dir() {
        for entry in std::fs::read_dir(&staging_src).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".md") {
                continue;
            }
            let dest_rel = format!("todo/staging/{name}");
            let dest = brain_root.join("todo").join("staging").join(&name);
            if dest.exists() {
                report.skipped.push((dest_rel.clone(), "already exists".to_string()));
                continue;
            }
            if execute {
                std::fs::create_dir_all(dest.parent().expect("staging has a parent")).ok();
                if let Err(e) = std::fs::copy(entry.path(), &dest) {
                    report.errors.push((dest_rel.clone(), e.to_string()));
                    continue;
                }
            }
            report.imported.push((dest_rel.clone(), dest_rel));
        }
    }

    // Report skipped files with their reasons.
    for (name, reason) in SKIPPED {
        if wolf_dir.join(name).is_file() {
            report.skipped.push((name.to_string(), reason.to_string()));
        }
    }

    // Anything else at the top level is neither imported nor skipped: name it
    // and leave it in place, so the report claims completeness by listing,
    // not by omission.
    let known = |name: &str| {
        MIGRATIONS.iter().any(|(m, _, _)| *m == name)
            || name == "buglog.json"
            || SKIPPED.iter().any(|(s, _)| *s == name)
            || name == "todo"
            || is_archive(name)
    };
    let mut unrecognized: Vec<String> = std::fs::read_dir(wolf_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            if known(&name) {
                String::new()
            } else if entry.path().is_dir() {
                format!("{name}/")
            } else {
                name
            }
        })
        .filter(|name| !name.is_empty())
        .collect();
    unrecognized.sort();
    report.unrecognized = unrecognized;

    Ok(report)
}

/// The dry run: the exact report the real run would produce, nothing written.
pub fn plan_openwolf(wolf_dir: &Path, brain_root: &Path) -> anyhow::Result<ImportReport> {
    collect(wolf_dir, brain_root, false)
}

pub fn import_openwolf(wolf_dir: &Path, brain_root: &Path) -> anyhow::Result<ImportReport> {
    collect(wolf_dir, brain_root, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_frontmatter_is_prepended_only_when_absent() {
        assert_eq!(with_ring("hello", Some(2)), "---\nring: 2\n---\n\nhello");
        assert_eq!(with_ring("---\ntitle: x\n---\n\nbody", Some(2)), "---\ntitle: x\n---\n\nbody");
        assert_eq!(with_ring("no ring needed", None), "no ring needed");
    }

    #[test]
    fn archive_files_are_recognised() {
        assert!(is_archive("handoff-archiv-bis-2026-08-06.md"));
        assert!(is_archive("memory.md.vor-aufraeumen"));
        assert!(is_archive("re-notes-archiv-2026-08-06.md"));
        assert!(!is_archive("cerebrum.md"));
        assert!(!is_archive("memory.md"));
        assert!(!is_archive("OPENWOLF.md"));
    }

    #[test]
    fn import_copies_and_skips_correctly() {
        let wolf = tempfile::tempdir().unwrap();
        let brain = tempfile::tempdir().unwrap();
        // Content files.
        std::fs::write(wolf.path().join("OPENWOLF.md"), "# Context\nRules here.").unwrap();
        std::fs::write(wolf.path().join("cerebrum.md"), "# Knowledge\nFacts here.").unwrap();
        std::fs::write(wolf.path().join("memory.md"), "# Memory\nBehaviours.").unwrap();
        // A file cfetch skips.
        std::fs::write(wolf.path().join("anatomy.md"), "# Anatomy\ncode index").unwrap();
        // An archive file.
        std::fs::write(wolf.path().join("handoff-archiv-2026-01-01.md"), "# Old handoff").unwrap();
        // A staging candidate.
        std::fs::create_dir_all(wolf.path().join("todo").join("staging")).unwrap();
        std::fs::write(wolf.path().join("todo/staging/candidate-abc.md"), "# Candidate").unwrap();

        let report = import_openwolf(wolf.path(), brain.path()).unwrap();

        assert!(report.imported.iter().any(|(s, d)| s == "OPENWOLF.md" && d == "AGENT.md"));
        assert!(report.imported.iter().any(|(s, d)| s == "cerebrum.md" && d == "knowledge/cerebrum.md"));
        assert!(report.imported.iter().any(|(s, d)| s == "memory.md" && d == "mind/memories/MEMORY.md"));
        assert!(report.imported.iter().any(|(s, _)| s.starts_with("handoff-archiv")));
        assert!(report.imported.iter().any(|(s, _)| s.starts_with("todo/staging/")));
        assert!(report.skipped.iter().any(|(s, r)| s == "anatomy.md" && r.contains("code index")));

        // Ring frontmatter was applied.
        let memory = std::fs::read_to_string(brain.path().join("mind/memories/MEMORY.md")).unwrap();
        assert!(memory.starts_with("---\nring: 2\n---"), "got: {}", &memory[..40.min(memory.len())]);
        let agent = std::fs::read_to_string(brain.path().join("AGENT.md")).unwrap();
        assert!(agent.starts_with("---\nring: 1\n---"));
    }

    #[test]
    fn dry_run_reports_exactly_what_the_real_run_does() {
        let wolf = tempfile::tempdir().unwrap();
        std::fs::write(wolf.path().join("cerebrum.md"), "# Knowledge\nsee bug-001.").unwrap();
        std::fs::write(
            wolf.path().join("buglog.json"),
            r#"{"version":1,"bugs":[{"id":"bug-001","timestamp":"2026-07-24","error_message":"frames decoded to noise","file":"loader.py","root_cause":"offsets were direct","fix":"use raw offsets","tags":["cwr"],"related_bugs":[],"occurrences":2,"last_seen":"2026-07-25"}]}"#,
        )
        .unwrap();
        std::fs::write(wolf.path().join("anatomy.md"), "index").unwrap();
        std::fs::write(wolf.path().join("ENGINE.md"), "live instructions").unwrap();

        let planned = plan_openwolf(wolf.path(), tempfile::tempdir().unwrap().path()).unwrap();
        let executed = import_openwolf(wolf.path(), tempfile::tempdir().unwrap().path()).unwrap();

        assert_eq!(planned.imported, executed.imported);
        assert_eq!(planned.skipped, executed.skipped);
        assert_eq!(planned.unrecognized, executed.unrecognized);
        assert_eq!(planned.errors, executed.errors);
        // Both runs see the skip and the unknown file.
        assert!(planned.skipped.iter().any(|(s, _)| s == "anatomy.md"));
        assert_eq!(planned.unrecognized, vec!["ENGINE.md".to_string()]);
    }

    #[test]
    fn dry_run_writes_nothing() {
        let wolf = tempfile::tempdir().unwrap();
        let brain = tempfile::tempdir().unwrap();
        std::fs::write(wolf.path().join("cerebrum.md"), "# Knowledge").unwrap();
        let report = plan_openwolf(wolf.path(), brain.path()).unwrap();
        assert_eq!(report.imported.len(), 1);
        assert!(!brain.path().join("knowledge/cerebrum.md").exists());
    }

    #[test]
    fn buglog_becomes_one_greppable_note_per_entry() {
        let wolf = tempfile::tempdir().unwrap();
        let brain = tempfile::tempdir().unwrap();
        std::fs::write(
            wolf.path().join("buglog.json"),
            r#"{"version":1,"bugs":[
                {"id":"bug-001","timestamp":"2026-07-24","error_message":"frames decoded to noise","file":"loader.py","root_cause":"offsets were direct","fix":"use raw offsets","tags":["cwr","sprites"],"related_bugs":["bug-002"],"occurrences":1,"last_seen":"2026-07-24"},
                {"id":"bug-002","timestamp":"2026-07-25","error_message":"stale assembly","file":"Godot","root_cause":"stale build","fix":"rebuild with --build-solutions","tags":[],"related_bugs":[],"occurrences":1,"last_seen":"2026-07-25"}
            ]}"#,
        )
        .unwrap();

        let report = import_openwolf(wolf.path(), brain.path()).unwrap();
        assert!(report
            .imported
            .iter()
            .any(|(s, d)| s == "buglog.json" && d == "knowledge/bugs/ (2 entries)"));

        let note = std::fs::read_to_string(brain.path().join("knowledge/bugs/bug-001.md")).unwrap();
        assert!(note.starts_with("---\nring: 3\n---"));
        assert!(note.contains("# bug-001 — frames decoded to noise"));
        assert!(note.contains("**file**: loader.py"));
        assert!(note.contains("**tags**: cwr, sprites"));
        assert!(note.contains("related: bug-002"));
        assert!(note.contains("## Root cause"));
        assert!(note.contains("## Fix"));
        assert!(note.contains("use raw offsets"));
        assert!(brain.path().join("knowledge/bugs/bug-002.md").exists());
    }

    #[test]
    fn a_broken_buglog_is_an_error_not_an_abort() {
        let wolf = tempfile::tempdir().unwrap();
        let brain = tempfile::tempdir().unwrap();
        std::fs::write(wolf.path().join("buglog.json"), "not json").unwrap();
        std::fs::write(wolf.path().join("memory.md"), "# Memory").unwrap();

        let report = import_openwolf(wolf.path(), brain.path()).unwrap();
        assert!(report.errors.iter().any(|(s, _)| s == "buglog.json"));
        assert!(report.imported.iter().any(|(s, _)| s == "memory.md"));
    }

    #[test]
    fn an_empty_buglog_is_a_skip_not_an_import() {
        let wolf = tempfile::tempdir().unwrap();
        let brain = tempfile::tempdir().unwrap();
        std::fs::write(wolf.path().join("buglog.json"), r#"{"version":1,"bugs":[]}"#).unwrap();
        let report = import_openwolf(wolf.path(), brain.path()).unwrap();
        assert!(report.skipped.iter().any(|(s, r)| s == "buglog.json" && r == "no entries"));
        assert!(!report.imported.iter().any(|(s, _)| s == "buglog.json"));
    }

    #[test]
    fn unknown_top_level_entries_are_named_not_dropped() {
        let wolf = tempfile::tempdir().unwrap();
        let brain = tempfile::tempdir().unwrap();
        std::fs::write(wolf.path().join("cerebrum.md"), "# Knowledge").unwrap();
        std::fs::write(wolf.path().join("anatomy.md"), "index").unwrap();
        std::fs::write(wolf.path().join("ENGINE.md"), "live instructions").unwrap();
        std::fs::write(wolf.path().join("PREREG_2026-08-01_abc.md"), "# prereg").unwrap();
        std::fs::write(wolf.path().join("PREREG_hashes.txt"), "abc123").unwrap();
        std::fs::create_dir_all(wolf.path().join("proposals")).unwrap();
        std::fs::write(wolf.path().join("proposals/p1.md"), "# Proposal").unwrap();

        let report = import_openwolf(wolf.path(), brain.path()).unwrap();
        assert!(report.unrecognized.contains(&"ENGINE.md".to_string()));
        assert!(report.unrecognized.contains(&"PREREG_2026-08-01_abc.md".to_string()));
        assert!(report.unrecognized.contains(&"PREREG_hashes.txt".to_string()));
        assert!(report.unrecognized.contains(&"proposals/".to_string()));
        // Known names are not reported as unknown.
        assert!(!report.unrecognized.iter().any(|n| n == "cerebrum.md"));
        assert!(!report.unrecognized.iter().any(|n| n == "anatomy.md"));
        // Nothing unrecognized was touched.
        assert!(wolf.path().join("ENGINE.md").is_file());
        assert!(wolf.path().join("proposals/p1.md").is_file());
    }
}
