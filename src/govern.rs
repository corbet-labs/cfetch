//! The governance loop's producers: what gets queued, and with which text.
//!
//! Economics (see DESIGN, "Reminder queue drained at UserPromptSubmit"): a
//! Stop-level `additionalContext` forces a whole extra model turn, so nothing
//! here ever emits at Stop. Producers only QUEUE into the session state; the
//! UserPromptSubmit hook drains the queue piggybacked on the next user prompt
//! at zero extra turns. Cadence re-injection is grounded in the
//! instruction-decay study cited in DESIGN (~5.6% compliance decay per
//! generated function): every N-th post-tool event re-surfaces the top ring-0
//! rules in ~100 tokens.

use std::path::{Component, Path, PathBuf};

use crate::config::Config;
use crate::resident::SessionScope;
use crate::session_state::SessionState;

/// ~100 tokens at the chars/3.5 heuristic — hard cap on one rule refresh.
const RULES_MAX_CHARS: usize = 350;
/// At most this many ring-0 descriptions per refresh.
const RULES_MAX_LINES: usize = 3;
/// Brain-tree writes at which the stale-STATUS nudge arms.
const STATUS_NUDGE_MIN_WRITES: usize = 3;

/// Stale-STATUS nudge: the session wrote `STATUS_NUDGE_MIN_WRITES`+ files
/// under the brain tree, none of them a `todo/active/*/STATUS.md` — queue a
/// reminder to update the task STATUS. Returns whether anything was queued.
pub fn queue_status_nudge(st: &mut SessionState, brain_root: &Path) -> bool {
    let mut brain_writes = 0usize;
    let mut touched_status = false;
    for p in &st.written {
        let path = Path::new(p);
        let Ok(rel) = path.strip_prefix(brain_root) else { continue };
        brain_writes += 1;
        touched_status |= is_active_status(rel);
    }
    if brain_writes < STATUS_NUDGE_MIN_WRITES || touched_status {
        return false;
    }
    st.queue_reminder(
        "status",
        &format!(
            "[cfetch: {brain_writes} brain file(s) written this session, but no todo/active/*/STATUS.md among them — update the task STATUS]"
        ),
    )
}

/// Capture-visibility: staged candidates are invisible by design (rings 5-6
/// are never injected), so their COUNT is surfaced instead — with the command
/// that shows them. The count covers the whole shared staging directory, so a
/// candidate another host flagged is announced here too. Returns whether
/// anything was queued.
pub fn queue_staging_visibility(st: &mut SessionState, staging_dir: &Path) -> bool {
    let n = crate::staging::pending_count(staging_dir);
    if n < 1 {
        return false;
    }
    st.queue_reminder(
        "staging",
        &format!("[cfetch: {n} staged candidate(s) await distillation — cfetch staging list]"),
    )
}

/// The top ring-0 rules as one "[cfetch rule refresh]" block: description
/// frontmatter lines of the resident config's ring-0 files THIS session is
/// entitled to (or, with no resident config at all, of the brain's
/// `mind/memories` files declaring `ring: 0`), first `RULES_MAX_LINES`,
/// capped at `RULES_MAX_CHARS`.
pub fn top_ring0_rules(cfg: &Config, scope: &SessionScope) -> Option<String> {
    let descriptions = if cfg.resident.is_empty() {
        memories_ring0_descriptions(&cfg.brain_root, &behavior_dirs(cfg))
    } else {
        resident_ring0_descriptions(cfg, scope)
    };
    if descriptions.is_empty() {
        return None;
    }
    let mut text = String::from("[cfetch rule refresh]");
    for d in descriptions {
        text.push_str("\n- ");
        text.push_str(&d);
    }
    if text.len() > RULES_MAX_CHARS {
        let mut cut = RULES_MAX_CHARS;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    Some(text)
}

/// Descriptions of the resident config's ring-0 files, in config order,
/// filtered by the session's injection scope. Unreadable files and files
/// without a description contribute nothing.
fn resident_ring0_descriptions(cfg: &Config, scope: &SessionScope) -> Vec<String> {
    let mut out = Vec::new();
    for entry in cfg
        .resident
        .iter()
        .filter(|e| e.ring == 0 && e.scope.matches(&scope.host, scope.repo.as_deref()))
    {
        if out.len() == RULES_MAX_LINES {
            break;
        }
        let Ok(raw) = std::fs::read_to_string(cfg.resolve(&entry.path)) else { continue };
        if let Some(d) = frontmatter_value(&raw, "description") {
            out.push(d);
        }
    }
    out
}

/// The directories the configured taxonomy puts behavioral memory in: every
/// ring rule naming a SUBTREE (a prefix ending in `/`) on rings 0-2. With the
/// shipped rules that is the distilled-memories directory; with a custom
/// taxonomy it is whatever that taxonomy calls the same thing — the fallback
/// follows the config instead of assuming one tree's layout.
fn behavior_dirs(cfg: &Config) -> Vec<PathBuf> {
    cfg.ring_rules
        .iter()
        .filter(|r| r.ring <= 2 && r.prefix.ends_with('/'))
        .map(|r| PathBuf::from(r.prefix.trim_end_matches('/')))
        .collect()
}

/// Descriptions of files declaring `ring: 0` under `dirs`, in path order
/// (read_dir order is arbitrary; the "first 3" must be deterministic).
fn memories_ring0_descriptions(brain_root: &Path, dirs: &[PathBuf]) -> Vec<String> {
    let mut files: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(brain_root.join(dir)) else { continue };
        files.extend(
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("md")),
        );
    }
    files.sort();
    let mut out = Vec::new();
    for f in files {
        if out.len() == RULES_MAX_LINES {
            break;
        }
        let Ok(raw) = std::fs::read_to_string(&f) else { continue };
        let ring0 = frontmatter_value(&raw, "ring")
            .is_some_and(|v| v.split_whitespace().next() == Some("0"));
        if !ring0 {
            continue;
        }
        if let Some(d) = frontmatter_value(&raw, "description") {
            out.push(d);
        }
    }
    out
}

/// `rel` (brain-root-relative) is exactly `todo/active/<task>/STATUS.md`.
fn is_active_status(rel: &Path) -> bool {
    let parts: Vec<&str> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    matches!(parts.as_slice(), ["todo", "active", _, "STATUS.md"])
}

/// Single-line value of `key:` from a leading `---` frontmatter block. Key
/// match is ASCII-case-insensitive; the first non-empty value wins. An
/// unterminated block yields nothing (mirrors the index's fail-closed
/// frontmatter handling).
fn frontmatter_value(text: &str, key: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return None;
    }
    let mut value = None;
    for line in lines {
        let t = line.trim();
        if t == "---" {
            return value;
        }
        if value.is_none()
            && let Some((k, v)) = t.split_once(':')
            && k.trim().eq_ignore_ascii_case(key)
        {
            let v = v.trim();
            if !v.is_empty() {
                value = Some(v.to_string());
            }
        }
    }
    None // unterminated frontmatter: fail closed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session matching no particular host or repo — enough for every
    /// unscoped entry, which is what these fixtures use.
    fn any_session() -> SessionScope {
        SessionScope { host: "any-host".into(), repo: None }
    }
    use crate::config::{Config, GovernanceConfig, ResidentEntry, Scope};
    use crate::exhaust;
    use serde_json::json;

    fn write_event(session: &str, path: &str) -> crate::hook_io::HookEvent {
        crate::hook_io::HookEvent {
            session_id: Some(session.into()),
            tool_name: Some("Write".into()),
            tool_input: Some(json!({"file_path": path})),
            ..Default::default()
        }
    }

    /// A staging directory holding exactly `n` candidates, produced through
    /// the real capture + trap path (hot-file: the same ring-3 brain file
    /// written in two distinct sessions).
    fn staged_tree(dir: &Path, n: usize) -> PathBuf {
        let brain = Path::new("/b/agents");
        let ex = exhaust::Exhaust::new(
            dir.join("logs/cfetch"),
            dir.join("staging/cfetch"),
            "h1".into(),
            1 << 20,
        );
        for i in 0..n {
            let path = format!("/b/agents/knowledge/hot{i}.md");
            for s in ["s1", "s2"] {
                ex.capture_post_tool(
                    &write_event(s, &path),
                    brain,
                    &crate::config::RingRules::default(),
                )
                .unwrap();
            }
        }
        ex.record_stop("s2").unwrap();
        ex.staging_dir
    }

    #[test]
    fn status_nudge_requires_three_brain_writes() {
        let brain = Path::new("/b/agents");
        let mut st = SessionState::default();
        st.record_write("/b/agents/knowledge/a.md");
        st.record_write("/b/agents/knowledge/b.md");
        st.record_write("/elsewhere/c.md");
        assert!(!queue_status_nudge(&mut st, brain), "two brain writes are below the threshold");
        assert!(st.drain_reminders().is_empty());
        st.record_write("/b/agents/mind/memories/c.md");
        assert!(queue_status_nudge(&mut st, brain));
        let texts = st.drain_reminders();
        assert_eq!(texts.len(), 1);
        assert!(texts[0].contains("STATUS"), "the text must point at the task STATUS: {}", texts[0]);
    }

    #[test]
    fn status_nudge_suppressed_when_status_was_written() {
        let brain = Path::new("/b/agents");
        let mut st = SessionState::default();
        st.record_write("/b/agents/knowledge/a.md");
        st.record_write("/b/agents/knowledge/b.md");
        st.record_write("/b/agents/todo/active/cfetch/STATUS.md");
        assert!(!queue_status_nudge(&mut st, brain));
        assert!(st.drain_reminders().is_empty());
    }

    #[test]
    fn status_nudge_ignores_lookalike_status_paths() {
        // A STATUS.md outside todo/active/<task>/ does not count as updating
        // the task STATUS.
        let brain = Path::new("/b/agents");
        let mut st = SessionState::default();
        st.record_write("/b/agents/knowledge/a.md");
        st.record_write("/b/agents/todo/done/old/STATUS.md");
        st.record_write("/b/agents/todo/active/STATUS.md"); // no task dir
        assert!(queue_status_nudge(&mut st, brain));
    }

    #[test]
    fn active_status_path_shape() {
        assert!(is_active_status(Path::new("todo/active/cfetch/STATUS.md")));
        assert!(!is_active_status(Path::new("todo/active/STATUS.md")));
        assert!(!is_active_status(Path::new("todo/done/cfetch/STATUS.md")));
        assert!(!is_active_status(Path::new("todo/active/cfetch/notes/STATUS.md")));
        assert!(!is_active_status(Path::new("todo/active/cfetch/status.md")));
    }

    #[test]
    fn staging_visibility_counts_pending_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let staging = staged_tree(dir.path(), 2);
        let mut st = SessionState::default();
        assert!(queue_staging_visibility(&mut st, &staging));
        assert_eq!(
            st.drain_reminders(),
            vec!["[cfetch: 2 staged candidate(s) await distillation — cfetch staging list]"
                .to_string()]
        );
    }

    #[test]
    fn staging_visibility_silent_when_nothing_is_staged() {
        // No staging directory at all: nothing queued, and the probe must not
        // create one.
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging/cfetch");
        let mut st = SessionState::default();
        assert!(!queue_staging_visibility(&mut st, &staging));
        assert!(!staging.exists(), "a read-only probe must not create tree state");
        // A queue whose candidates were all consumed: also silent.
        let staging = staged_tree(dir.path(), 1);
        let id = crate::staging::list(&staging)[0].id.clone();
        crate::staging::consume(&staging, &id).unwrap();
        assert!(!queue_staging_visibility(&mut st, &staging));
        assert!(st.drain_reminders().is_empty());
    }

    #[test]
    fn frontmatter_value_parses_and_fails_closed() {
        let doc = "---\nname: feedback_x\ndescription: never do the thing\nring: 0\n---\nbody\n";
        assert_eq!(frontmatter_value(doc, "description"), Some("never do the thing".into()));
        assert_eq!(frontmatter_value(doc, "ring"), Some("0".into()));
        assert_eq!(frontmatter_value(doc, "absent"), None);
        assert_eq!(frontmatter_value("no frontmatter here", "description"), None);
        assert_eq!(
            frontmatter_value("---\ndescription: unterminated\nbody", "description"),
            None,
            "an unterminated block must yield nothing"
        );
        assert_eq!(frontmatter_value("---\ndescription:\n---\n", "description"), None);
        assert_eq!(
            frontmatter_value("---\nDescription: mixed case\n---\n", "description"),
            Some("mixed case".into())
        );
    }

    #[test]
    fn ring0_rules_come_from_resident_ring0_files_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("guards.md"),
            "---\ndescription: destruction is human-gated\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("policy.md"),
            "---\ndescription: policy line must not appear\n---\nbody\n",
        )
        .unwrap();
        let cfg = Config {
            brain_root: dir.path().to_path_buf(),
            resident: vec![
                ResidentEntry { path: PathBuf::from("guards.md"), ring: 0, scope: Scope::default() },
                ResidentEntry { path: PathBuf::from("policy.md"), ring: 1, scope: Scope::default() },
            ],
            ..Config::default()
        };
        let rules = top_ring0_rules(&cfg, &any_session()).unwrap();
        assert!(rules.starts_with("[cfetch rule refresh]"));
        assert!(rules.contains("destruction is human-gated"));
        assert!(!rules.contains("policy line"), "ring-1 files are not rule-refresh material");
    }

    #[test]
    fn ring0_rules_fall_back_to_memories_when_resident_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let memories = dir.path().join("mind/memories");
        std::fs::create_dir_all(&memories).unwrap();
        std::fs::write(
            memories.join("feedback_a.md"),
            "---\nring: 0\ndescription: rule alpha\n---\n",
        )
        .unwrap();
        std::fs::write(
            memories.join("feedback_b.md"),
            "---\nring: 2\ndescription: behavior beta\n---\n",
        )
        .unwrap();
        std::fs::write(memories.join("feedback_c.md"), "---\nring: 0\n---\nno description\n")
            .unwrap();
        std::fs::write(
            memories.join("feedback_d.md"),
            "---\nring: 0\ndescription: rule delta\n---\n",
        )
        .unwrap();
        let cfg = Config {
            brain_root: dir.path().to_path_buf(),
            resident: Vec::new(),
            ..Config::default()
        };
        let rules = top_ring0_rules(&cfg, &any_session()).unwrap();
        assert!(rules.contains("rule alpha"));
        assert!(rules.contains("rule delta"));
        assert!(!rules.contains("behavior beta"), "ring-2 memories must not surface");
    }

    #[test]
    fn ring0_rules_fallback_follows_custom_ring_rules() {
        // A tree that calls its behavioral memory something else: the
        // fallback reads the config's subtree, not a hardcoded one.
        let dir = tempfile::tempdir().unwrap();
        let handbook = dir.path().join("handbook");
        std::fs::create_dir_all(&handbook).unwrap();
        std::fs::write(
            handbook.join("a.md"),
            "---\nring: 0\ndescription: house rule alpha\n---\n",
        )
        .unwrap();
        let memories = dir.path().join("mind/memories");
        std::fs::create_dir_all(&memories).unwrap();
        std::fs::write(
            memories.join("b.md"),
            "---\nring: 0\ndescription: not this tree\n---\n",
        )
        .unwrap();
        let cfg = Config {
            brain_root: dir.path().to_path_buf(),
            resident: Vec::new(),
            ring_rules: vec![crate::config::RingRule { prefix: "handbook/".into(), ring: 1 }],
            ..Config::default()
        };
        let rules = top_ring0_rules(&cfg, &any_session()).unwrap();
        assert!(rules.contains("house rule alpha"));
        assert!(!rules.contains("not this tree"), "the shipped layout has no privilege here");
    }

    #[test]
    fn ring0_rules_cap_lines_and_chars() {
        let dir = tempfile::tempdir().unwrap();
        let memories = dir.path().join("mind/memories");
        std::fs::create_dir_all(&memories).unwrap();
        for i in 0..5 {
            std::fs::write(
                memories.join(format!("feedback_{i}.md")),
                format!("---\nring: 0\ndescription: rule number {i} {}\n---\n", "x".repeat(200)),
            )
            .unwrap();
        }
        let cfg = Config {
            brain_root: dir.path().to_path_buf(),
            resident: Vec::new(),
            ..Config::default()
        };
        let rules = top_ring0_rules(&cfg, &any_session()).unwrap();
        assert!(rules.len() <= 350, "rule refresh was {} chars for a ~100-token cap", rules.len());
        assert!(!rules.contains("rule number 3"), "only the first 3 descriptions are taken");
        assert!(!rules.contains("rule number 4"));
    }

    #[test]
    fn no_ring0_material_yields_none() {
        let dir = tempfile::tempdir().unwrap();
        // Resident configured but nothing at ring 0.
        std::fs::write(dir.path().join("p.md"), "---\ndescription: d\n---\n").unwrap();
        let cfg = Config {
            brain_root: dir.path().to_path_buf(),
            resident: vec![ResidentEntry { path: PathBuf::from("p.md"), ring: 1, scope: Scope::default() }],
            ..Config::default()
        };
        assert_eq!(top_ring0_rules(&cfg, &any_session()), None);
        // No resident config and no memories dir at all.
        let cfg = Config {
            brain_root: dir.path().to_path_buf(),
            resident: Vec::new(),
            ..Config::default()
        };
        assert_eq!(top_ring0_rules(&cfg, &any_session()), None);
    }

    #[test]
    fn governance_config_is_carried_by_default() {
        // Compile-time guard that the block exists on Config; behavior gating
        // lives in the hooks layer tests.
        let g = GovernanceConfig::default();
        assert!(g.enabled);
        assert_eq!(g.reinject_every, 25);
    }
}
