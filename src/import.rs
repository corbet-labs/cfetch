//! One-way importer from openwolf-enhanced `.wolf/` directories into the
//! cfetch brain tree. Preserves every markdown file an operator or agent
//! wrote; skips everything cfetch regenerates (code index, token ledger,
//! exhaust, embeddings, hooks, cron state).
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
//! | archive `*.md` | `knowledge/archive/` | excl | out of the index |
//! | `todo/staging/*` | `todo/staging/*` | 5 | quarantine, same contract |
//!
//! Files cfetch derives on its own are skipped with a note: `anatomy.md`
//! (code index), `buglog.json` (ring-6 exhaust traps), `token-ledger.json`,
//! `STATUS.md`, `config.json` (cfetch has its own two-layer config),
//! `recall-embeddings.*` (cfetch will re-embed), hooks, logs, cron state.

use std::path::Path;

pub struct ImportReport {
    pub imported: Vec<(String, String)>,
    pub skipped: Vec<(String, String)>,
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
    ("buglog.json", "cfetch tracks errors via ring-6 exhaust"),
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

pub fn import_openwolf(wolf_dir: &Path, brain_root: &Path) -> anyhow::Result<ImportReport> {
    anyhow::ensure!(
        wolf_dir.is_dir(),
        "{} is not a directory; point at the .wolf/ directory",
        wolf_dir.display()
    );
    let mut report = ImportReport { imported: Vec::new(), skipped: Vec::new(), errors: Vec::new() };

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
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("create {}: {e}", parent.display()))?;
        }
        let raw = std::fs::read_to_string(&src)
            .map_err(|e| anyhow::anyhow!("read {}: {e}", src.display()))?;
        std::fs::write(&dest, with_ring(&raw, *ring))
            .map_err(|e| anyhow::anyhow!("write {}: {e}", dest.display()))?;
        report.imported.push((src_name.to_string(), dest_rel.to_string()));
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
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            match std::fs::copy(&src, &dest) {
                Ok(_) => report.imported.push((name.clone(), format!("knowledge/archive/{}", name))),
                Err(e) => report.errors.push((name.clone(), e.to_string())),
            }
        }
    }

    // Migrate staging candidates (quarantine in cfetch has the same contract).
    let staging_src = wolf_dir.join("todo").join("staging");
    if staging_src.is_dir() {
        let staging_dest = brain_root.join("todo").join("staging");
        std::fs::create_dir_all(&staging_dest).ok();
        for entry in std::fs::read_dir(&staging_src).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".md") {
                continue;
            }
            let dest = staging_dest.join(&name);
            if dest.exists() {
                report.skipped.push((format!("todo/staging/{}", name), "already exists".to_string()));
                continue;
            }
            match std::fs::copy(entry.path(), &dest) {
                Ok(_) => report.imported.push((format!("todo/staging/{}", name), format!("todo/staging/{}", name))),
                Err(e) => report.errors.push((format!("todo/staging/{}", name), e.to_string())),
            }
        }
    }

    // Report skipped files with their reasons.
    for (name, reason) in SKIPPED {
        if wolf_dir.join(name).is_file() {
            report.skipped.push((name.to_string(), reason.to_string()));
        }
    }

    Ok(report)
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
}
