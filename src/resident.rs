//! The resident set: ring-0/1 content injected unconditionally at session
//! start. Budget-clipped with honest truncation markers; private blocks are
//! removed fail-closed (an unclosed <private> swallows to end of input —
//! degrade to MORE private, never less).

use std::fmt::Write as _;

use crate::config::Config;

/// Removes `<private>...</private>` regions. Fail-closed: an opening tag with
/// no closing tag removes everything from the tag to the end.
pub fn strip_private(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        match rest.find("<private>") {
            None => {
                out.push_str(rest);
                return out;
            }
            Some(start) => {
                out.push_str(&rest[..start]);
                let after = &rest[start + "<private>".len()..];
                match after.find("</private>") {
                    None => return out, // fail closed
                    Some(end) => rest = &after[end + "</private>".len()..],
                }
            }
        }
    }
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

    let budget = cfg.budget_chars.max(200);
    let share = budget / sections.len();
    let mut text = String::new();
    let mut sources = Vec::new();
    for (label, body) in sections {
        let clipped = if body.len() > share {
            let mut cut = share.saturating_sub(80).max(80);
            while cut < body.len() && !body.is_char_boundary(cut) {
                cut += 1;
            }
            format!(
                "{}\n[clipped at {} of {} chars — full content: {}]",
                &body[..cut.min(body.len())],
                cut.min(body.len()),
                body.len(),
                label
            )
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
    fn digest_clips_to_budget_with_marker() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.md");
        std::fs::write(&big, "line\n".repeat(5000)).unwrap();
        let cfg = Config {
            brain_root: dir.path().to_path_buf(),
            resident: vec![ResidentEntry { path: PathBuf::from("big.md"), ring: 0 }],
            budget_chars: 1000,
            ledger_max_sessions: 10,
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
            budget_chars: 1000,
            ledger_max_sessions: 10,
        };
        let d = build(&cfg);
        assert!(d.text.contains("resident file missing"));
    }
}
