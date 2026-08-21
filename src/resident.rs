//! The resident set: ring-0/1 content injected at session start, per POLICY —
//! each entry declares the sessions it belongs to and only those receive it.
//! Budget-clipped with honest truncation markers; private blocks are removed
//! fail-closed (an unclosed <private> swallows to end of input — degrade to
//! MORE private, never less).

use std::fmt::Write as _;
use std::path::Path;

use crate::config::Config;
use crate::hook_io::HookEvent;

const OPEN: &str = "<private>";
const CLOSE: &str = "</private>";

/// Byte ranges (tags inclusive) of private regions, DEPTH-AWARE: a nested
/// `<private>` does not let the first `</private>` end the region — trailing
/// private content must never leak. Unbalanced opens fail closed to end of
/// input; a stray close outside any region is plain text.
fn private_regions(s: &str) -> Vec<(usize, usize)> {
    let mut regions = Vec::new();
    let mut depth = 0usize;
    let mut region_start = 0usize;
    let mut pos = 0usize;
    while pos < s.len() {
        let next_open = s[pos..].find(OPEN).map(|i| pos + i);
        let next_close = s[pos..].find(CLOSE).map(|i| pos + i);
        let open_first = match (next_open, next_close) {
            (None, None) => break,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some(o), Some(c)) => o < c,
        };
        if open_first {
            let o = next_open.unwrap();
            if depth == 0 {
                region_start = o;
            }
            depth += 1;
            pos = o + OPEN.len();
        } else {
            let c = next_close.unwrap();
            if depth > 0 {
                depth -= 1;
                if depth == 0 {
                    regions.push((region_start, c + CLOSE.len()));
                }
            }
            pos = c + CLOSE.len();
        }
    }
    if depth > 0 {
        regions.push((region_start, s.len())); // fail closed
    }
    regions
}

/// Like `strip_private`, but replaces private content with spaces instead of
/// removing it, preserving newlines — so line numbers in citations computed
/// from the blanked text still match the file on disk.
pub fn blank_private(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut cursor = 0usize;
    for (start, end) in private_regions(s) {
        out.push_str(&s[cursor..start]);
        for c in s[start..end].chars() {
            out.push(if c == '\n' { '\n' } else { ' ' });
        }
        cursor = end;
    }
    out.push_str(&s[cursor..]);
    out
}

/// Removes `<private>...</private>` regions (nesting-aware, fail-closed).
pub fn strip_private(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut cursor = 0usize;
    for (start, end) in private_regions(s) {
        out.push_str(&s[cursor..start]);
        cursor = end;
    }
    out.push_str(&s[cursor..]);
    out
}

/// The session an injection decision is made for. Two coordinates, because
/// those are the two an agent session actually has at SessionStart: which
/// machine it runs on, and what it is working on.
#[derive(Debug, Clone)]
pub struct SessionScope {
    pub host: String,
    /// The working directory's own name — the repo, as the agent sees it.
    pub repo: Option<String>,
}

impl SessionScope {
    /// From a hook event. The repo name is the LAST component of the event's
    /// `cwd`: no filesystem walk, because the hook path must not stat its way
    /// up a tree that may be an NFS mount.
    pub fn from_event(event: &HookEvent) -> SessionScope {
        SessionScope::from_cwd(event.cwd.as_deref())
    }

    pub fn from_cwd(cwd: Option<&str>) -> SessionScope {
        SessionScope { host: crate::paths::hostname(), repo: repo_name(cwd) }
    }

    /// For non-hook callers (selfcheck): this process's own working directory.
    pub fn current() -> SessionScope {
        let cwd = std::env::current_dir().ok();
        SessionScope::from_cwd(cwd.as_deref().and_then(Path::to_str))
    }
}

fn repo_name(cwd: Option<&str>) -> Option<String> {
    let trimmed = cwd?.trim_end_matches('/');
    let name = Path::new(trimmed).file_name()?.to_string_lossy().to_string();
    (!name.is_empty()).then_some(name)
}

pub struct ResidentDigest {
    pub text: String,
    /// (source label, chars contributed) — selfcheck reporting and, from
    /// Milestone 5, per-source injection booking.
    pub sources: Vec<(String, usize)>,
    /// Labels of the entries this session's scope excluded. Reported rather
    /// than dropped: a resident file that stops arriving must be explainable
    /// without reading the config.
    pub skipped_by_scope: Vec<String>,
}

/// Builds the injected digest for ONE session. Entries whose scope does not
/// match the session are left out entirely — they cost no budget and are not
/// booked. Each surviving file gets a proportional share of the budget; a
/// file over its share is clipped with a marker naming the file so the model
/// knows where the rest lives.
pub fn build(cfg: &Config, scope: &SessionScope) -> ResidentDigest {
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut skipped_by_scope: Vec<String> = Vec::new();
    for entry in &cfg.resident {
        let label = format!("ring-{} {}", entry.ring, entry.path.display());
        if !entry.scope.matches(&scope.host, scope.repo.as_deref()) {
            skipped_by_scope.push(label);
            continue;
        }
        let path = cfg.resolve(&entry.path);
        match std::fs::read_to_string(&path) {
            Ok(raw) => {
                let clean = strip_private(&raw);
                if !clean.trim().is_empty() {
                    sections.push((label, clean.trim().to_string()));
                }
            }
            Err(_) => {
                // A missing resident file is worth one short line, not silence:
                // the resident set is the contract the operator configured.
                sections.push((label.clone(), format!("[resident file missing: {}]", path.display())));
            }
        }
    }

    if sections.is_empty() {
        return ResidentDigest { text: String::new(), sources: Vec::new(), skipped_by_scope };
    }

    // The budget is a HARD cap on the whole digest: headers and clip markers
    // are charged against it, not added on top.
    let budget = cfg.budget_chars.max(200);
    let overhead: usize = sections.iter().map(|(label, _)| label.len() + 8).sum();
    let share = budget.saturating_sub(overhead).max(sections.len() * 60) / sections.len();
    let mut text = String::new();
    let mut sources = Vec::new();
    for (label, body) in sections {
        let clipped = if body.len() > share {
            let marker_reserve = 60 + label.len();
            let mut cut = share.saturating_sub(marker_reserve).max(40).min(body.len());
            while cut < body.len() && !body.is_char_boundary(cut) {
                cut += 1;
            }
            if cut < body.len() {
                format!(
                    "{}\n[clipped at {cut} of {} chars — full content: {label}]",
                    &body[..cut],
                    body.len(),
                )
            } else {
                body
            }
        } else {
            body
        };
        let _ = write!(text, "== {label} ==\n{clipped}\n\n");
        sources.push((label, clipped.len()));
    }
    ResidentDigest { text: text.trim_end().to_string(), sources, skipped_by_scope }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ResidentEntry, Scope};
    use std::path::PathBuf;

    /// A brain with four resident files and the scopes the injection policy
    /// is meant to discriminate on.
    fn scoped_cfg(dir: &std::path::Path) -> Config {
        for name in ["everywhere.md", "on-host.md", "in-repo.md", "elsewhere.md"] {
            std::fs::write(dir.join(name), format!("body of {name}\n")).unwrap();
        }
        Config {
            brain_root: dir.to_path_buf(),
            resident: vec![
                ResidentEntry {
                    path: PathBuf::from("everywhere.md"),
                    ring: 1,
                    scope: Scope::default(),
                },
                ResidentEntry {
                    path: PathBuf::from("on-host.md"),
                    ring: 1,
                    scope: Scope { hosts: vec!["build-box".into()], ..Scope::default() },
                },
                ResidentEntry {
                    path: PathBuf::from("in-repo.md"),
                    ring: 1,
                    scope: Scope { repos: vec!["widget".into()], ..Scope::default() },
                },
                ResidentEntry {
                    path: PathBuf::from("elsewhere.md"),
                    ring: 1,
                    scope: Scope {
                        hosts: vec!["other-box".into()],
                        repos: vec!["gadget".into()],
                        always: false,
                    },
                },
            ],
            ..Config::default()
        }
    }

    #[test]
    fn session_scope_reads_the_repo_from_the_hook_event_cwd() {
        let event: HookEvent =
            serde_json::from_str(r#"{"session_id":"s1","cwd":"/srv/work/widget"}"#).unwrap();
        let scope = SessionScope::from_event(&event);
        assert_eq!(scope.repo.as_deref(), Some("widget"));
        assert!(!scope.host.is_empty(), "the host is always known");

        let trailing: HookEvent = serde_json::from_str(r#"{"cwd":"/srv/work/widget/"}"#).unwrap();
        assert_eq!(SessionScope::from_event(&trailing).repo.as_deref(), Some("widget"));

        let no_cwd: HookEvent = serde_json::from_str(r#"{"session_id":"s1"}"#).unwrap();
        assert!(SessionScope::from_event(&no_cwd).repo.is_none());
    }

    #[test]
    fn injection_selects_by_host_scope() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = scoped_cfg(dir.path());
        let scope = SessionScope { host: "build-box".into(), repo: Some("sprocket".into()) };
        let d = build(&cfg, &scope);
        assert!(d.text.contains("body of everywhere.md"), "an unscoped entry is always in");
        assert!(d.text.contains("body of on-host.md"), "the host matches");
        assert!(!d.text.contains("body of in-repo.md"), "wrong repo");
        assert!(!d.text.contains("body of elsewhere.md"), "neither host nor repo matches");
        assert_eq!(d.skipped_by_scope.len(), 2, "skips are reported, never silent");
    }

    #[test]
    fn injection_selects_by_repo_scope() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = scoped_cfg(dir.path());
        let scope = SessionScope { host: "laptop".into(), repo: Some("widget".into()) };
        let d = build(&cfg, &scope);
        assert!(d.text.contains("body of everywhere.md"));
        assert!(d.text.contains("body of in-repo.md"), "the repo matches");
        assert!(!d.text.contains("body of on-host.md"));
        assert!(!d.text.contains("body of elsewhere.md"));
    }

    #[test]
    fn a_session_matching_nothing_still_gets_the_unscoped_entries() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = scoped_cfg(dir.path());
        let scope = SessionScope { host: "laptop".into(), repo: None };
        let d = build(&cfg, &scope);
        assert!(d.text.contains("body of everywhere.md"));
        assert_eq!(d.sources.len(), 1, "only the unscoped entry is booked");
        assert_eq!(d.skipped_by_scope.len(), 3);
    }

    #[test]
    fn scoped_out_entries_do_not_consume_the_budget() {
        // The share each injected file gets is computed over the entries that
        // actually reach the session, never over the whole configured list.
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = scoped_cfg(dir.path());
        for name in ["everywhere.md", "on-host.md", "in-repo.md", "elsewhere.md"] {
            std::fs::write(dir.path().join(name), "word ".repeat(4000)).unwrap();
        }
        cfg.budget_chars = 2000;
        let all = build(&cfg, &SessionScope { host: "build-box".into(), repo: Some("widget".into()) });
        let one = build(&cfg, &SessionScope { host: "laptop".into(), repo: None });
        assert_eq!(all.sources.len(), 3, "everywhere + host + repo; `elsewhere` matches neither");
        assert_eq!(one.sources.len(), 1);
        assert!(one.text.len() <= 2000, "the budget is still a hard cap");
        assert!(
            one.sources[0].1 > all.sources[0].1,
            "the surviving entry gets the whole budget: {} vs {}",
            one.sources[0].1,
            all.sources[0].1
        );
    }

    #[test]
    fn private_blocks_are_removed() {
        assert_eq!(strip_private("a<private>secret</private>b"), "ab");
        assert_eq!(strip_private("plain"), "plain");
    }

    #[test]
    fn unclosed_private_swallows_to_end() {
        assert_eq!(strip_private("keep<private>oops no close\nmore"), "keep");
    }

    #[test]
    fn multiple_private_blocks() {
        assert_eq!(strip_private("a<private>x</private>b<private>y</private>c"), "abc");
    }

    #[test]
    fn nested_private_blocks_do_not_leak_the_tail() {
        // The first </private> must NOT close the outer region.
        let s = "a<private>x<private>y</private>STILL-PRIVATE</private>b";
        assert_eq!(strip_private(s), "ab");
        let b = blank_private(s);
        assert!(!b.contains("STILL-PRIVATE"));
        assert_eq!(b.len(), s.len());
    }

    #[test]
    fn stray_close_tag_is_plain_text() {
        assert_eq!(strip_private("a</private>b"), "a</private>b");
    }

    #[test]
    fn digest_budget_is_a_hard_cap() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.md", "b.md", "c.md"] {
            std::fs::write(dir.path().join(name), "word ".repeat(3000)).unwrap();
        }
        let cfg = Config {
            brain_root: dir.path().to_path_buf(),
            resident: ["a.md", "b.md", "c.md"]
                .iter()
                .map(|n| ResidentEntry { path: PathBuf::from(n), ring: 1, scope: Scope::default() })
                .collect(),
            code_roots: Vec::new(),
            budget_chars: 2000,
            ..Config::default()
        };
        let d = build(&cfg, &SessionScope { host: "any-host".into(), repo: None });
        assert!(d.text.len() <= 2000, "digest was {} chars for a 2000 budget", d.text.len());
        assert_eq!(d.text.matches("[clipped at ").count(), 3);
    }

    #[test]
    fn blanking_preserves_length_and_newlines() {
        let s = "keep\n<private>zqx\nwvy</private>\ntail";
        let b = blank_private(s);
        assert_eq!(b.len(), s.len());
        assert_eq!(b.matches('\n').count(), s.matches('\n').count());
        assert!(b.starts_with("keep\n"));
        assert!(b.ends_with("\ntail"));
        assert!(!b.contains("zqx") && !b.contains("wvy"));
    }

    #[test]
    fn blanking_unclosed_blanks_to_end_keeping_newlines() {
        let s = "keep\n<private>x\ny";
        let b = blank_private(s);
        assert_eq!(b.len(), s.len());
        assert!(b.starts_with("keep\n"));
        assert!(!b.contains('x'));
        assert!(!b.contains('y'));
        assert_eq!(b.matches('\n').count(), 2);
    }

    #[test]
    fn digest_clips_to_budget_with_marker() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.md");
        std::fs::write(&big, "line\n".repeat(5000)).unwrap();
        let cfg = Config {
            brain_root: dir.path().to_path_buf(),
            resident: vec![ResidentEntry { path: PathBuf::from("big.md"), ring: 0, scope: Scope::default() }],
            code_roots: Vec::new(),
            budget_chars: 1000,
            ..Config::default()
        };
        let d = build(&cfg, &SessionScope { host: "any-host".into(), repo: None });
        assert!(d.text.len() < 1400, "digest was {} chars", d.text.len());
        assert!(d.text.contains("[clipped at "));
    }

    #[test]
    fn missing_file_yields_one_line_not_silence() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            brain_root: dir.path().to_path_buf(),
            resident: vec![ResidentEntry { path: PathBuf::from("absent.md"), ring: 1, scope: Scope::default() }],
            code_roots: Vec::new(),
            budget_chars: 1000,
            ..Config::default()
        };
        let d = build(&cfg, &SessionScope { host: "any-host".into(), repo: None });
        assert!(d.text.contains("resident file missing"));
    }
}
