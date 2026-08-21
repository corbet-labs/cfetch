//! The resident set: ring-0/1 content injected unconditionally at session
//! start. Budget-clipped with honest truncation markers; private blocks are
//! removed fail-closed (an unclosed <private> swallows to end of input —
//! degrade to MORE private, never less).

use std::fmt::Write as _;

use crate::config::Config;

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

pub struct ResidentDigest {
    pub text: String,
    /// (source label, chars contributed) — selfcheck reporting and, from
    /// Milestone 5, per-source injection booking.
    pub sources: Vec<(String, usize)>,
}

/// Builds the injected digest. Each file gets a proportional share of the
/// budget; a file over its share is clipped with a marker naming the file so
/// the model knows where the rest lives.
pub fn build(cfg: &Config) -> ResidentDigest {
    let mut sections: Vec<(String, String)> = Vec::new();
    for entry in &cfg.resident {
        let path = cfg.resolve(&entry.path);
        let label = format!("ring-{} {}", entry.ring, entry.path.display());
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
        return ResidentDigest { text: String::new(), sources: Vec::new() };
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
    ResidentDigest { text: text.trim_end().to_string(), sources }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ResidentEntry;
    use std::path::PathBuf;

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
                .map(|n| ResidentEntry { path: PathBuf::from(n), ring: 1 })
                .collect(),
            code_roots: Vec::new(),
            budget_chars: 2000,
            ledger_max_sessions: 10,
            ..Config::default()
        };
        let d = build(&cfg);
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
            resident: vec![ResidentEntry { path: PathBuf::from("big.md"), ring: 0 }],
            code_roots: Vec::new(),
            budget_chars: 1000,
            ledger_max_sessions: 10,
            ..Config::default()
        };
        let d = build(&cfg);
        assert!(d.text.len() < 1400, "digest was {} chars", d.text.len());
        assert!(d.text.contains("[clipped at "));
    }

    #[test]
    fn missing_file_yields_one_line_not_silence() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            brain_root: dir.path().to_path_buf(),
            resident: vec![ResidentEntry { path: PathBuf::from("absent.md"), ring: 1 }],
            code_roots: Vec::new(),
            budget_chars: 1000,
            ledger_max_sessions: 10,
            ..Config::default()
        };
        let d = build(&cfg);
        assert!(d.text.contains("resident file missing"));
    }
}
