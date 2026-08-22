//! Structural condensation of oversized command output.
//!
//! Bash tool results are the single largest consumer of context — measured at
//! 48.3% of tool-result tokens in the lineage this mechanism comes from. A
//! 4,000-line `find` or a repository-wide grep enters the conversation whole,
//! is read once, and is then paid for on every subsequent cached turn.
//!
//! Four invariants, each of which exists because the naive version of this
//! feature destroys something:
//!
//! 1. **stderr is never touched.** It is small, it is where errors live, and
//!    condensing it would hide the thing the operator needs.
//! 2. **Test and build output is never rewritten.** The failing assertion is
//!    frequently in the middle of exactly the region a head/tail window would
//!    drop. These are advised on, not edited.
//! 3. **A savings floor.** Rewriting output to save 8% costs the reader their
//!    trust in what they are seeing, for nothing. Below the floor, the output
//!    passes through untouched.
//! 4. **Nothing is destroyed.** Condensation returns what was elided so the
//!    caller can persist it and leave a pointer. This module never decides on
//!    its own that information may be lost.

// The hook that would call this is PostToolUse[Bash], which lives in hooks.rs
// — a file a second agent is actively rewriting for Codex support. The module
// is complete and tested on its own; wiring it is one call site, and doing it
// while that rewrite is in flight would mean two agents editing one function.
// Landed separately on purpose.
#![allow(dead_code)]

/// What kind of command produced this output. The family decides the strategy,
/// because the useful part of `git status` is at the top and the useful part of
/// a stack trace is at the bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// Tests and builds — advisory only, never rewritten.
    Verification,
    /// Long flat lists where the middle is uninformative: find, ls, grep, rg.
    Listing,
    /// Version control: the summary is at the top, the detail below.
    Vcs,
    /// Anything else.
    Generic,
}

impl Family {
    /// May output of this family be rewritten at all?
    pub fn rewritable(self) -> bool {
        self != Family::Verification
    }
}

/// Classifies a command line by its leading program, following `sudo`, `env`
/// and simple assignments. Deliberately conservative: an unrecognized command
/// is Generic, never assumed safe to treat specially.
pub fn classify(command: &str) -> Family {
    let mut words = command.split_whitespace().peekable();
    // Step over prefixes that are not the real program.
    while let Some(w) = words.peek() {
        let w = *w;
        if w == "sudo" || w == "env" || w == "time" || w == "nohup" || w.contains('=') {
            words.next();
        } else {
            break;
        }
    }
    let Some(prog) = words.next() else { return Family::Generic };
    // Strip any path: /usr/bin/rg is rg.
    let prog = prog.rsplit(['/', '\\']).next().unwrap_or(prog);
    // The subcommand comes from the SAME iterator, after the prefixes were
    // stepped over — reading it off the raw word list makes `RUST_LOG=x cargo
    // test` look like the subcommand is `cargo`.
    let sub = words.next().unwrap_or("");

    match prog {
        // Verification: the payload is the failure, wherever it sits.
        "cargo" if matches!(sub, "test" | "build" | "clippy" | "check" | "bench") => {
            Family::Verification
        }
        "pytest" | "jest" | "vitest" | "go" | "make" | "ninja" | "gradle" | "mvn" | "tox" => {
            Family::Verification
        }
        "npm" | "pnpm" | "yarn" | "bun" if matches!(sub, "test" | "run" | "build" | "ci") => {
            Family::Verification
        }
        "git" => Family::Vcs,
        // Listings: long, flat, and uninformative in the middle.
        "find" | "ls" | "grep" | "rg" | "ag" | "fd" | "tree" | "du" | "locate" => Family::Listing,
        // PowerShell and cmd equivalents, so a Windows session gets the same
        // treatment rather than silently falling through to Generic.
        "Get-ChildItem" | "gci" | "dir" | "Select-String" | "sls" => Family::Listing,
        _ => Family::Generic,
    }
}

/// The outcome of considering some output for condensation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condensed {
    /// What should enter the conversation.
    pub text: String,
    /// Lines removed from the middle. Empty when nothing was removed — the
    /// caller persists these and leaves a pointer, so nothing is ever lost.
    pub elided: Vec<String>,
    /// Why the output was left alone, when it was.
    pub untouched_because: Option<&'static str>,
}

impl Condensed {
    fn untouched(text: &str, why: &'static str) -> Condensed {
        Condensed { text: text.to_string(), elided: Vec::new(), untouched_because: Some(why) }
    }

    pub fn was_condensed(&self) -> bool {
        !self.elided.is_empty()
    }
}

/// Lines kept from the head and the tail of a condensed listing.
const HEAD: usize = 40;
const TAIL: usize = 20;

/// Below this many lines, nothing is worth condensing.
const MIN_LINES: usize = HEAD + TAIL + 10;

/// A rewrite must save at least this fraction of the bytes, or it is not worth
/// the reader's suspicion.
const SAVINGS_FLOOR: f64 = 0.30;

/// Considers `output` for condensation, given the command that produced it.
///
/// Returns the original text unchanged — with a reason — whenever condensing
/// would be wrong or pointless, so the caller has a single code path.
pub fn condense(command: &str, output: &str) -> Condensed {
    let family = classify(command);
    if !family.rewritable() {
        return Condensed::untouched(
            output,
            "test and build output is advised on, never rewritten — the failure is often \
             exactly what a head/tail window would drop",
        );
    }
    let lines: Vec<&str> = output.lines().collect();
    if lines.len() < MIN_LINES {
        return Condensed::untouched(output, "short enough to pass through");
    }

    let keep_tail = match family {
        // A VCS summary is at the top; the tail is usually the least useful
        // part, so keep less of it.
        Family::Vcs => TAIL / 2,
        _ => TAIL,
    };
    let elided: Vec<String> =
        lines[HEAD..lines.len() - keep_tail].iter().map(|s| s.to_string()).collect();

    let marker = format!("… {} line(s) elided by cfetch …", elided.len());
    let mut kept: Vec<&str> = Vec::with_capacity(HEAD + keep_tail + 1);
    kept.extend_from_slice(&lines[..HEAD]);
    kept.push(&marker);
    kept.extend_from_slice(&lines[lines.len() - keep_tail..]);
    let text = kept.join("\n");

    // The floor is measured on the actual bytes, at the point of rewriting —
    // the only number that is defensible afterwards.
    let saved = output.len().saturating_sub(text.len()) as f64 / output.len().max(1) as f64;
    if saved < SAVINGS_FLOOR {
        return Condensed::untouched(output, "condensing would not save enough to be worth it");
    }
    Condensed { text, elided, untouched_because: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(n: usize) -> String {
        (0..n).map(|i| format!("line {i} with enough text to matter")).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn the_program_is_found_past_prefixes_and_paths() {
        assert_eq!(classify("rg pattern src/"), Family::Listing);
        assert_eq!(classify("/usr/bin/rg pattern"), Family::Listing);
        assert_eq!(classify("sudo find / -name x"), Family::Listing);
        assert_eq!(classify("RUST_LOG=debug cargo test"), Family::Verification);
        assert_eq!(classify("env FOO=1 git status"), Family::Vcs);
        assert_eq!(classify(""), Family::Generic);
        assert_eq!(classify("some-unknown-tool --flags"), Family::Generic);
    }

    #[test]
    fn windows_listing_commands_are_recognised_too() {
        // A PowerShell session must get the same treatment rather than
        // silently falling through to Generic.
        assert_eq!(classify("Get-ChildItem -Recurse"), Family::Listing);
        assert_eq!(classify("Select-String -Pattern foo"), Family::Listing);
        assert_eq!(classify("dir /s"), Family::Listing);
    }

    #[test]
    fn verification_output_is_never_rewritten_however_long() {
        // The failing assertion is frequently in the middle — exactly what a
        // head/tail window drops.
        let out = lines(5000);
        let c = condense("cargo test --all", &out);
        assert!(!c.was_condensed());
        assert_eq!(c.text, out, "byte-identical");
        assert!(c.untouched_because.unwrap().contains("never rewritten"));

        for cmd in ["pytest -q", "make -j8", "npm run build", "go test ./..."] {
            assert!(!condense(cmd, &out).was_condensed(), "{cmd}");
        }
    }

    #[test]
    fn a_long_listing_is_condensed_and_nothing_is_lost() {
        let out = lines(2000);
        let c = condense("find . -type f", &out);
        assert!(c.was_condensed());
        assert!(c.text.len() < out.len() / 2);
        // Everything removed is handed back, so the caller can persist it.
        assert_eq!(c.elided.len(), 2000 - HEAD - TAIL);
        let kept = c.text.lines().count();
        assert_eq!(kept, HEAD + 1 + TAIL, "head + marker + tail");
        // The first and last lines survive: those are the ones that orient.
        assert!(c.text.starts_with("line 0 "));
        assert!(c.text.trim_end().ends_with("line 1999 with enough text to matter"));
        assert!(c.text.contains("elided by cfetch"));
    }

    #[test]
    fn short_output_passes_through_untouched() {
        let out = lines(12);
        let c = condense("ls -la", &out);
        assert!(!c.was_condensed());
        assert_eq!(c.text, out);
    }

    #[test]
    fn a_rewrite_that_would_not_save_enough_is_declined() {
        // Many lines, but almost all the bytes are in the head we keep — so
        // the saving is below the floor and the output passes through.
        let mut v: Vec<String> = (0..HEAD).map(|i| format!("{i} {}", "x".repeat(4000))).collect();
        v.extend((0..MIN_LINES).map(|i| format!("{i}")));
        let out = v.join("\n");
        let c = condense("find .", &out);
        assert!(!c.was_condensed(), "the floor should have declined this");
        assert!(c.untouched_because.unwrap().contains("not save enough"));
    }

    #[test]
    fn the_elided_lines_are_exactly_the_ones_removed() {
        // The contract the caller relies on to persist a faithful copy.
        let out = lines(500);
        let c = condense("rg foo", &out);
        let all: Vec<&str> = out.lines().collect();
        assert_eq!(c.elided, all[HEAD..all.len() - TAIL].iter().map(|s| s.to_string()).collect::<Vec<_>>());
    }
}
