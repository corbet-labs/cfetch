//! `cfetch init`: create the brain tree cfetch's defaults already describe.
//!
//! Until now those defaults — `mind/memories/` is ring 2, `todo/` is ring 4,
//! `mind/secrets/` is never indexed — described a layout cfetch did not
//! create. That made them a description of ONE tree that happened to exist,
//! and left a new user with a binary whose rules matched nothing they had.
//! Creating the tree turns the same constants into a specification.
//!
//! Two rules govern what is created:
//!
//! - The TOP-LEVEL directories are the standard. They are what a ring rule, a
//!   slice and a grant can name, so they must mean the same thing on every
//!   machine or none of those three is portable.
//! - The SUBDIRECTORIES are not. `knowledge/` holds whatever subjects a person
//!   has, `todo/` whatever lanes they work in, `logs/` whichever agent clients
//!   they run. Inventing those would be prescribing someone's life.
//!
//! A few subdirectory NAMES are nonetheless reserved, because a rule already
//! keys on them. They are documented rather than created: if they exist the
//! rule applies, and if they never exist nothing is lost.

use std::path::{Path, PathBuf};

/// One created directory and the README that explains it. The README is the
/// point: a directory whose purpose is only recorded in a distant tool's
/// source is a directory people put the wrong things in.
struct Dir {
    name: &'static str,
    readme: &'static str,
}

/// Top-level directories. Order is the order they are reported in.
const DIRS: &[Dir] = &[
    Dir {
        name: "knowledge",
        readme: "\
# knowledge

What is true, by subject. Ring 3 — recallable, never injected unasked.

Subdirectories are yours: one per subject you actually have. Nothing here is
created for you, because a subject list is a description of a life.

Reserved name: `archive/` — retired knowledge. Kept for provenance, and held
out of ordinary recall, so a superseded decision cannot answer a question as
though it were current.
",
    },
    Dir {
        name: "mind",
        readme: "\
# mind

How to behave, and what grants access.

- `memories/` — distilled behaviour, ring 2. `memories/MEMORY.md` is the index
  and is ring 1: it is read every session, so it is the one file whose size is
  a standing cost.
- `secrets/` — RESERVED. Never indexed, never recalled, never injected, at any
  configuration. cfetch refuses this prefix rather than merely defaulting to
  skipping it.
- `skills/`, `tools/`, `config/` — procedures and executables. Recallable.

Subdirectories beyond those are yours.
",
    },
    Dir {
        name: "todo",
        readme: "\
# todo

Work state. Ring 4 — the current task, not the finished record.

Lanes are yours: whatever you actually move work through.

Reserved name: `scratch/` — high-volume disposable working files. NEVER
indexed. Without this exclusion a scratch directory drowns the ring it shares:
a real tree measured 12,276 scratch files against 27 files of live task state,
and every query paid for the ratio.
",
    },
    Dir {
        name: "logs",
        readme: "\
# logs

Ring 6: raw exhaust, per agent client. Never injected, never committed.

One subdirectory per client you run — `claude/`, `codex/`, `gemini/`, and so
on. Which ones exist is yours; the shape is the standard, so that a question
like \"which client costs least for this work\" has one answer across machines.

Transcripts may embed plaintext secrets. This directory is excluded from git
by the .gitignore written alongside it, and exclusion means \"no history\", not
\"no backup\".
",
    },
    Dir {
        name: "staging",
        readme: "\
# staging

Ring 5: candidates awaiting distillation.

Written by cfetch when a capture looks worth keeping. Never recallable and
never injected — the LOCATION decides that, so a candidate whose frontmatter
was stripped or hand-edited is still quarantined.

Promotion out of here is a deliberate act. Nothing is promoted automatically.
",
    },
    Dir {
        name: "state",
        readme: "\
# state

No ring: nothing here is a statement, so nothing here can be recalled,
injected, or contradict anything. Derived artifacts that are expensive enough
to share but are not the record.

Vector embeddings live here, keyed by content hash, so a document is embedded
once per storage group rather than once per machine. Delete any of it and it
rebuilds; the markdown is the truth.

Not committed: it is derived, and it is large.
",
    },
];

/// Subdirectory names a rule already keys on. Documented in the parent's
/// README, never created — a reserved name costs nothing while unused, and
/// creating it would prescribe a workflow.
pub const RESERVED: &[(&str, &str)] = &[
    ("mind/secrets", "never indexed at any configuration"),
    ("todo/scratch", "never indexed: high-volume disposable working files"),
    ("knowledge/archive", "indexed, held out of ordinary recall"),
];

const GITIGNORE: &str = "\
# Written by `cfetch init`. Exclusion here means \"no history\", not \"no backup\":
# everything below still lives on the tree and rides whatever snapshots it has.

# Ring 6. Per-client transcripts may embed plaintext secrets verbatim.
/logs/

# Derived, and large. Rebuilt from the markdown at any time.
/state/

# Ring 5. Candidates are working material, not record — they graduate into the
# tree by a deliberate promotion, and until then they are not history.
/staging/
";

const ROOT_README: &str = "\
# The brain

A markdown tree that agents read, write and recall from. The markdown IS the
record: everything cfetch derives from it — indexes, embeddings, graphs — is a
cache that can be deleted and rebuilt.

Privilege runs by ring: 0 and 1 are operator invariants and policy, 2 is
distilled behaviour, 3 is knowledge, 4 is work state, 5 is unpromoted
candidates, 6 is raw exhaust. A lower ring wins a contradiction, and the ring
of a statement is visible in the citation that carries it.

The top-level directories are a standard, so that a slice shared with someone
else means the same thing on both ends. What goes inside them is yours.

By convention `projects/` holds code, and is excluded from the prose index —
source is reached through `cfetch find`, which reads symbols, not paragraphs.
";

pub struct Created {
    pub root: PathBuf,
    pub dirs: Vec<(String, bool)>,
    pub files: Vec<(String, bool)>,
}

/// Create the tree at `root`. Additive and idempotent: an existing directory
/// is left alone and an existing file is NEVER overwritten, because this may
/// be run against a tree someone already keeps and losing their AGENT.md to a
/// scaffold would be unforgivable.
pub fn run(root: &Path) -> anyhow::Result<Created> {
    let mut created = Created { root: root.to_path_buf(), dirs: Vec::new(), files: Vec::new() };
    std::fs::create_dir_all(root)?;

    for dir in DIRS {
        let path = root.join(dir.name);
        let fresh = !path.exists();
        std::fs::create_dir_all(&path)?;
        created.dirs.push((dir.name.to_string(), fresh));
        created.files.push(write_if_absent(&path.join("README.md"), dir.readme)?);
    }
    created.files.push(write_if_absent(&root.join("README.md"), ROOT_README)?);
    created.files.push(write_if_absent(&root.join(".gitignore"), GITIGNORE)?);
    Ok(created)
}

/// `(relative name, was written)`. Never truncates: a false here means the
/// file was already someone's.
fn write_if_absent(path: &Path, body: &str) -> anyhow::Result<(String, bool)> {
    let label = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().to_string());
    let shown = path
        .parent()
        .and_then(Path::file_name)
        .map_or(label.clone(), |p| format!("{}/{}", p.to_string_lossy(), label));
    if path.exists() {
        return Ok((shown, false));
    }
    crate::fsutil::atomic_write(path, body)?;
    Ok((shown, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_top_level_directory_gets_a_readme_naming_its_ring_or_its_rule() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(dir.path()).unwrap();
        assert_eq!(out.dirs.len(), DIRS.len());
        for d in DIRS {
            let readme = dir.path().join(d.name).join("README.md");
            assert!(readme.exists(), "{} has no README", d.name);
            let body = std::fs::read_to_string(&readme).unwrap();
            assert!(
                body.contains("Ring") || body.contains("ring") || body.contains("indexed"),
                "{}'s README explains neither its ring nor its rule",
                d.name
            );
        }
    }

    /// The whole point of the standard: a rule, a slice and a grant all name
    /// a top-level directory, so the set cfetch creates and the set its rules
    /// key on cannot be allowed to drift apart.
    #[test]
    fn the_created_tree_covers_every_directory_the_default_rules_name() {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path()).unwrap();
        for named in ["mind", "todo", "staging", "logs", "knowledge", "state"] {
            assert!(dir.path().join(named).is_dir(), "{named} is named by a rule but not created");
        }
    }

    /// Reserved names are documented, never created — creating them would
    /// prescribe a workflow, and leaving them undocumented would make the rule
    /// that keys on them invisible.
    #[test]
    fn reserved_subdirectories_are_documented_but_not_created() {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path()).unwrap();
        for (path, _) in RESERVED {
            assert!(!dir.path().join(path).exists(), "{path} must not be created");
            let (parent, child) = path.split_once('/').unwrap();
            let readme = std::fs::read_to_string(dir.path().join(parent).join("README.md")).unwrap();
            assert!(readme.contains(child), "{parent}/README.md never mentions {child}");
        }
    }

    /// This may be run against a tree someone already keeps.
    #[test]
    fn a_second_run_changes_nothing_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let mine = dir.path().join("knowledge/README.md");
        std::fs::create_dir_all(mine.parent().unwrap()).unwrap();
        std::fs::write(&mine, "my own words").unwrap();

        let first = run(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&mine).unwrap(), "my own words");
        assert!(
            first.files.iter().any(|(n, written)| n.ends_with("README.md") && !*written),
            "an existing file must be reported as untouched"
        );

        let second = run(dir.path()).unwrap();
        assert!(second.dirs.iter().all(|(_, fresh)| !fresh), "a second run creates no directory");
        assert!(second.files.iter().all(|(_, written)| !*written), "a second run writes no file");
    }
}
