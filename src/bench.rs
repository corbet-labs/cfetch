//! A/B bench: does cfetch pay for itself?
//!
//! Every other measurement in this binary prices what cfetch COSTS (the
//! ledger books injected characters, `cfetch audit` prices the always-on
//! bill). None of them can say whether the bill bought anything. That answer
//! only exists as a difference between two runs of the same task — one with
//! cfetch installed, one without — so this module refuses to compute it from
//! a single arm, and refuses to compute it at all below the point where the
//! arithmetic stops being anecdote.
//!
//! Everything here is read back out of the harness's own transcripts, which
//! is the only record both arms leave: a bare run has no ledger, no hooks and
//! no daemon to report anything. Three consequences shape the design:
//!
//! - **The arm is read, never assumed.** A transcript carrying a verified
//!   cfetch injection is the cfetch arm. A transcript that never mentions
//!   cfetch anywhere in its bytes is the bare arm. Everything between the two
//!   — hooks that fired but delivered nothing, a session where the agent
//!   merely typed `cfetch` — is a measurement gap and is reported as one. A
//!   bench that guessed the arm would be measuring its own guess.
//! - **Pairs, not averages.** Session cost is dominated by which task ran,
//!   not by which arm ran it, so the headline figure is the median of
//!   per-pair differences, a pair being two sessions whose FIRST user prompt
//!   is identical. Per-arm medians are printed too, labeled with the task-mix
//!   confound they carry.
//! - **The re-run detector.** cfetch condenses oversized command output. If
//!   condensation throws away something the agent needed, the agent runs the
//!   command again — so the share of a session's shell calls that repeat an
//!   earlier identical command in the SAME session is a direct read on
//!   whether condensation is lossy. Memory that pays for itself pulls that
//!   share down; a cfetch arm that lands within a couple of points of the
//!   bare arm bought nothing with the tokens it spent. The share is POOLED
//!   over an arm's sessions rather than taken as a median of per-session
//!   shares: against 583 real sessions the per-session median is 0% while
//!   the pooled rate is ~3% (1401 repeats of 42960 shell calls), so a
//!   median-based detector would have read TIE in every arm forever. That
//!   same measurement bounds what the detector can resolve, and the verdict
//!   refuses itself when the control arm's own rate is inside the band.
//!
//! Below the thresholds the report says "could not measure" and names what is
//! missing. This project rejects savings-versus-bare-CLI numbers that were
//! not measured, and a bench that always produces a figure is exactly how one
//! gets manufactured.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::{paths, transcript};

/// Default lookback. Wider than the audit window: a bench arm is deliberately
/// run and then compared days later, and dropping the earlier arm would leave
/// the later one unpaired.
pub const DEFAULT_WINDOW_DAYS: u64 = 30;
/// A session that used the shell fewer times than this contributes neither
/// its calls nor its repeats: a two-command session carries no evidence about
/// re-fetching in either direction, and both arms should be judged on the
/// sessions that actually worked.
const MIN_SHELL_CALLS: usize = 5;
/// Fewer paired runs than this and the median of the differences is anecdote.
const MIN_PAIRS: usize = 3;
/// Same floor for the detector, required in EACH arm.
const MIN_RERUN_SESSIONS: usize = 3;
/// The detector's tie band, in percentage points: inside it, the two arms
/// re-fetched the same amount and cfetch's condensation paid for nothing.
const TIE_POINTS: f64 = 2.0;

const ARM_CFETCH: &str = "cfetch";
const ARM_BARE: &str = "bare";

/// The dimensions every arm is reported on, named once so the per-arm
/// medians, the paired deltas and the JSON can never drift apart.
const DIMENSIONS: [&str; 5] = [
    "api_calls",
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_created_tokens",
];

/// Transcript roots, overridable so tests run against a fabricated home
/// instead of the operator's real sessions.
pub struct BenchPaths {
    pub roots: Vec<(&'static str, PathBuf)>,
}

impl BenchPaths {
    pub fn defaults() -> BenchPaths {
        BenchPaths {
            roots: vec![
                (agent_session::AGENT_CLAUDE, paths::native_projects_root()),
                (agent_session::AGENT_CODEX, paths::codex_sessions_root()),
                (agent_session::AGENT_GEMINI, paths::gemini_sessions_root()),
                (agent_session::AGENT_CURSOR, paths::cursor_sessions_root()),
            ],
        }
    }
}

/// One dimension's figure. `None` is "nothing to measure here" — an arm with
/// no sessions must serialize as null and render as a dash, never as a zero
/// that reads like a measurement.
#[derive(Debug, Serialize)]
pub struct DimensionStat {
    pub dimension: &'static str,
    pub value: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct ArmStats {
    pub arm: &'static str,
    pub sessions: usize,
    /// Per-session medians, in `DIMENSIONS` order.
    pub medians: Vec<DimensionStat>,
    /// Sessions that cleared `MIN_SHELL_CALLS` and so feed the detector.
    pub rerun_sessions: usize,
    /// Shell calls and repeats POOLED across those sessions. The detector's
    /// rate is the pooled ratio, not the median of per-session ratios: on
    /// real transcripts most sessions repeat no command at all, so the
    /// per-session median sits at 0% in every arm and the comparison would
    /// read TIE forever — a detector that always fires is not a detector.
    /// The two totals are reported so the denominator is visible.
    pub shell_calls: u64,
    pub shell_repeats: u64,
    pub rerun_pct: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct PairedDelta {
    pub pairs: usize,
    /// Median of the per-pair `cfetch - bare` differences, `DIMENSIONS` order.
    pub medians: Vec<DimensionStat>,
}

/// The detector's answer. `NotMeasured` is a first-class outcome, not an
/// error path: too few sessions must never render as a zero difference.
#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "verdict", rename_all = "kebab-case")]
pub enum Rerun {
    NotMeasured {
        reason: String,
    },
    /// Within `TIE_POINTS` of the bare arm: condensation destroyed
    /// information the agent then had to fetch again.
    Tie {
        cfetch_pct: f64,
        bare_pct: f64,
        points_apart: f64,
    },
    /// The cfetch arm repeated fewer commands — memory replaced a re-fetch.
    Lower {
        cfetch_pct: f64,
        bare_pct: f64,
        points_apart: f64,
    },
    /// The cfetch arm repeated MORE commands than the bare arm.
    Higher {
        cfetch_pct: f64,
        bare_pct: f64,
        points_apart: f64,
    },
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub window_days: u64,
    /// Transcripts inside the window, before any of them were classified.
    pub scanned: usize,
    pub arms: Vec<ArmStats>,
    pub pairs_found: usize,
    /// `None` below `MIN_PAIRS`: too few pairs to state a difference.
    pub paired: Option<PairedDelta>,
    pub rerun: Rerun,
    /// Every transcript that was NOT counted, with the reason it was not.
    pub gaps: Vec<String>,
    pub method: Vec<&'static str>,
}

/// One measured session, reduced to what the bench compares.
struct Run {
    arm: &'static str,
    /// First user prompt hash; `None` when the transcript has no prompt, in
    /// which case the run still counts per arm but can never be paired.
    pair_key: Option<String>,
    dims: [u64; 5],
    shell_calls: usize,
    shell_repeats: usize,
}

impl Run {
    /// Whether this run's shell activity feeds the detector at all. A session
    /// under the floor is still a cost measurement; it just carries no rate.
    fn counts_for_rerun(&self) -> bool {
        self.shell_calls >= MIN_SHELL_CALLS
    }
}

/// Which arm a transcript belongs to. `Unknown` carries the reason so the
/// report can name what it failed to classify instead of dropping it.
enum Arm {
    Cfetch,
    Bare,
    Unknown(String),
}

/// Case-insensitive substring test that allocates nothing. Transcripts run to
/// megabytes and the negative half of the arm test reads every byte of every
/// one of them.
fn mentions(text: &str, needle: &str) -> bool {
    let n = needle.as_bytes();
    !n.is_empty() && text.as_bytes().windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// Arm classification, evidence-first.
///
/// The negative test is deliberately the crude one: if the six bytes of our
/// own name appear nowhere in the file, cfetch injected nothing, ran nothing
/// and was named by nobody — a conclusion no hook-record schema change can
/// invalidate. Only once the name IS present does the delicate question
/// ("was that an injection, or the agent typing our name?") arise, and that
/// question already has one answer in this codebase, so it is asked there.
fn arm_of(text: &str, path: &Path) -> Arm {
    if !mentions(text, "cfetch") {
        return Arm::Bare;
    }
    match transcript::verified_injections(path) {
        Some((_, delivered)) if delivered > 0 => Arm::Cfetch,
        Some((fired, _)) => Arm::Unknown(format!(
            "{fired} cfetch hook firing(s) delivered nothing — cfetch was paid for and contributed no context, which is neither arm"
        )),
        None => Arm::Unknown(
            "names cfetch but no injection could be verified (agent-typed mention, or hook-record format drift)"
                .into(),
        ),
    }
}

/// Whitespace-collapsed command text. "Identical" means identical: the
/// buglog normalizer used elsewhere in this binary folds every path token to
/// `<path>`, which would score reading two different files as a re-fetch.
fn command_key(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Shell calls and how many of them repeated an earlier one, vendor-neutral:
/// `category == "shell"` is agent-session's own classification, so Bash,
/// Codex `exec_command` and Cursor's shell tool all count.
///
/// A shell event whose command text did not survive parsing is skipped
/// outright rather than counted as an empty command — otherwise every such
/// event after the first would score as a repeat of the others and the
/// detector would manufacture its own signal.
fn shell_stats(session: &agent_session::AgentSession) -> (usize, usize) {
    let mut seen: HashSet<String> = HashSet::new();
    let (mut calls, mut repeats) = (0usize, 0usize);
    for tool in &session.events.tools {
        if tool.category != "shell" {
            continue;
        }
        let key = command_key(&tool.command);
        if key.is_empty() {
            continue;
        }
        calls += 1;
        if !seen.insert(key) {
            repeats += 1;
        }
    }
    (calls, repeats)
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = values.len();
    Some(if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    })
}

fn medians_of(runs: &[&Run]) -> Vec<DimensionStat> {
    DIMENSIONS
        .iter()
        .enumerate()
        .map(|(i, dimension)| {
            let mut values: Vec<f64> = runs.iter().map(|r| r.dims[i] as f64).collect();
            DimensionStat { dimension, value: median(&mut values) }
        })
        .collect()
}

/// Per-arm median for one dimension, reducing several same-arm runs of one
/// pair key to a single number before the pair is differenced. Never called
/// on an empty side: a pair exists only where both arms ran the task.
fn arm_median(runs: &[&Run], dimension: usize) -> f64 {
    let mut values: Vec<f64> = runs.iter().map(|r| r.dims[dimension] as f64).collect();
    median(&mut values).unwrap_or(0.0)
}

pub fn build(bench_paths: &BenchPaths, window_days: u64, now: SystemTime) -> Report {
    let cutoff = now.checked_sub(Duration::from_secs(window_days * 24 * 60 * 60));
    let mut runs: Vec<Run> = Vec::new();
    let mut gap_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut scanned = 0usize;

    for (agent, root) in &bench_paths.roots {
        for candidate in agent_session::discover_session_files_in_dir(agent, root) {
            if cutoff.is_some_and(|c| candidate.updated < c) {
                continue;
            }
            scanned += 1;
            let Ok(text) = std::fs::read_to_string(&candidate.path) else {
                *gap_counts.entry("transcript unreadable".into()).or_default() += 1;
                continue;
            };
            let arm = match arm_of(&text, &candidate.path) {
                Arm::Cfetch => ARM_CFETCH,
                Arm::Bare => ARM_BARE,
                Arm::Unknown(reason) => {
                    *gap_counts.entry(reason).or_default() += 1;
                    continue;
                }
            };
            // Usage goes through the binary's ONE usage extractor, which
            // knows which counters each harness records natively and returns
            // nothing rather than a measured-looking zero. Re-reading the
            // file is the price of not owning a second copy of that logic.
            let Some(usage) = transcript::scan(&candidate.path) else {
                *gap_counts
                    .entry("usage not measurable from this transcript (format drift)".into())
                    .or_default() += 1;
                continue;
            };
            let Some(session) = agent_session::parse_session_content(
                agent,
                &candidate.path,
                candidate.updated,
                &text,
            ) else {
                *gap_counts.entry("transcript events unparseable".into()).or_default() += 1;
                continue;
            };
            let (shell_calls, shell_repeats) = shell_stats(&session);
            runs.push(Run {
                arm,
                pair_key: session.events.prompts.first().map(|p| p.text_hash.clone()),
                dims: [
                    usage.api_calls,
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_read_input_tokens,
                    usage.cache_creation_input_tokens,
                ],
                shell_calls,
                shell_repeats,
            });
        }
    }

    let arms: Vec<ArmStats> = [ARM_CFETCH, ARM_BARE]
        .into_iter()
        .map(|arm| {
            let of_arm: Vec<&Run> = runs.iter().filter(|r| r.arm == arm).collect();
            let counting: Vec<&&Run> = of_arm.iter().filter(|r| r.counts_for_rerun()).collect();
            let calls: u64 = counting.iter().map(|r| r.shell_calls as u64).sum();
            let repeats: u64 = counting.iter().map(|r| r.shell_repeats as u64).sum();
            ArmStats {
                arm,
                sessions: of_arm.len(),
                medians: medians_of(&of_arm),
                rerun_sessions: counting.len(),
                shell_calls: calls,
                shell_repeats: repeats,
                rerun_pct: (calls > 0).then(|| repeats as f64 * 100.0 / calls as f64),
            }
        })
        .collect();

    let (pairs_found, paired) = paired_deltas(&runs);
    let rerun = rerun_verdict(&arms);

    Report {
        window_days,
        scanned,
        arms,
        pairs_found,
        paired,
        rerun,
        gaps: gap_counts.into_iter().map(|(reason, n)| format!("{n} session(s): {reason}")).collect(),
        method: method(),
    }
}

/// Groups runs by pair key and differences the arms within each key. Keys
/// present in only one arm are not pairs and contribute nothing.
fn paired_deltas(runs: &[Run]) -> (usize, Option<PairedDelta>) {
    let mut by_key: BTreeMap<&str, (Vec<&Run>, Vec<&Run>)> = BTreeMap::new();
    for run in runs {
        let Some(key) = run.pair_key.as_deref() else { continue };
        let slot = by_key.entry(key).or_default();
        if run.arm == ARM_CFETCH { slot.0.push(run) } else { slot.1.push(run) }
    }
    let pairs: Vec<(Vec<&Run>, Vec<&Run>)> = by_key
        .into_values()
        .filter(|(cfetch, bare)| !cfetch.is_empty() && !bare.is_empty())
        .collect();
    if pairs.len() < MIN_PAIRS {
        return (pairs.len(), None);
    }
    let medians = DIMENSIONS
        .iter()
        .enumerate()
        .map(|(i, dimension)| {
            let mut deltas: Vec<f64> = pairs
                .iter()
                .map(|(cfetch, bare)| arm_median(cfetch, i) - arm_median(bare, i))
                .collect();
            DimensionStat { dimension, value: median(&mut deltas) }
        })
        .collect();
    (pairs.len(), Some(PairedDelta { pairs: pairs.len(), medians }))
}

fn rerun_verdict(arms: &[ArmStats]) -> Rerun {
    let find = |name: &str| arms.iter().find(|a| a.arm == name);
    let (Some(cfetch), Some(bare)) = (find(ARM_CFETCH), find(ARM_BARE)) else {
        return Rerun::NotMeasured { reason: "both arms are required".into() };
    };
    let thin: Vec<String> = [cfetch, bare]
        .iter()
        .filter(|a| a.rerun_sessions < MIN_RERUN_SESSIONS)
        .map(|a| {
            format!(
                "{} arm has {} session(s) with at least {MIN_SHELL_CALLS} shell call(s), {MIN_RERUN_SESSIONS} required",
                a.arm, a.rerun_sessions
            )
        })
        .collect();
    if !thin.is_empty() {
        return Rerun::NotMeasured { reason: thin.join("; ") };
    }
    let (Some(cfetch_pct), Some(bare_pct)) = (cfetch.rerun_pct, bare.rerun_pct) else {
        return Rerun::NotMeasured { reason: "no shell calls in one of the arms".into() };
    };
    let delta = cfetch_pct - bare_pct;
    let points_apart = delta.abs();
    // A tie is only evidence when the control arm re-fetches MORE than the
    // band is wide. Measured over 583 real sessions the bare rate is ~3%, so
    // a control arm can sit inside the 2-point band on its own — and there
    // "within 2 points of bare" cannot tell cfetch eliminating every
    // re-fetch from cfetch changing nothing. Convicting condensation on that
    // would be the dead measurement this whole module exists to avoid.
    if points_apart <= TIE_POINTS && bare_pct <= TIE_POINTS {
        return Rerun::NotMeasured {
            reason: format!(
                "the bare arm repeated {}% of its commands ({} of {}), itself inside the {TIE_POINTS}-point band: the band is wider than the phenomenon, so no result here could separate cfetch removing every re-fetch from cfetch changing nothing",
                num(bare_pct),
                bare.shell_repeats,
                bare.shell_calls
            ),
        };
    }
    if points_apart <= TIE_POINTS {
        Rerun::Tie { cfetch_pct, bare_pct, points_apart }
    } else if delta < 0.0 {
        Rerun::Lower { cfetch_pct, bare_pct, points_apart }
    } else {
        Rerun::Higher { cfetch_pct, bare_pct, points_apart }
    }
}

/// Shipped with every report, in both renderings: a figure whose method is
/// not attached to it is the kind of number this project rejects.
fn method() -> Vec<&'static str> {
    vec![
        "arms are read from the transcript, never assumed: a verified cfetch injection is the cfetch arm, a transcript that never names cfetch is the bare arm, anything between the two is a gap below",
        "a pair is two sessions, one per arm, whose FIRST user prompt is identical; the paired median is the headline because session cost is dominated by which task ran",
        "re-run rate = an arm's shell calls that repeat a command already run in the same session, pooled over its sessions of at least 5 shell calls; two arms within 2 points of each other re-fetched the same amount",
        "this command measures runs you performed; it starts nothing. run the same prompt twice, once with cfetch installed and once with it removed",
    ]
}

fn num(v: f64) -> String {
    if v.fract() == 0.0 { format!("{}", v as i64) } else { format!("{v:.1}") }
}

pub fn render(r: &Report) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let w = &mut out;
    let _ = writeln!(
        w,
        "a/b bench — does cfetch pay for itself? (window: last {} days, {} transcript(s) scanned)",
        r.window_days, r.scanned
    );

    let _ = writeln!(w, "\nmethod:");
    for line in &r.method {
        let _ = writeln!(w, "  - {line}");
    }

    let cfetch = r.arms.iter().find(|a| a.arm == ARM_CFETCH);
    let bare = r.arms.iter().find(|a| a.arm == ARM_BARE);
    let _ = writeln!(w, "\narms (per-session medians; UNPAIRED, so task mix is inside these numbers):");
    let _ = writeln!(
        w,
        "  {:<22} {:>16} {:>16}",
        "dimension",
        format!("cfetch (n={})", cfetch.map_or(0, |a| a.sessions)),
        format!("bare (n={})", bare.map_or(0, |a| a.sessions)),
    );
    for (i, dimension) in DIMENSIONS.iter().enumerate() {
        let cell = |arm: Option<&ArmStats>| {
            arm.and_then(|a| a.medians.get(i))
                .and_then(|d| d.value)
                .map_or_else(|| "-".to_string(), num)
        };
        let _ = writeln!(w, "  {:<22} {:>16} {:>16}", dimension, cell(cfetch), cell(bare));
    }

    match &r.paired {
        Some(p) => {
            let _ = writeln!(
                w,
                "\npaired deltas (cfetch minus bare, median over {} pair(s)):",
                p.pairs
            );
            for d in &p.medians {
                let cell = d.value.map_or_else(|| "-".to_string(), num);
                let _ = writeln!(w, "  {:<22} {:>16}", d.dimension, cell);
            }
        }
        None => {
            let _ = writeln!(
                w,
                "\npaired deltas: COULD NOT MEASURE — {} pair(s) found, {MIN_PAIRS} required (a pair needs the same first prompt in both arms)",
                r.pairs_found
            );
        }
    }

    // The two rates are printed even when the verdict is refused: the reader
    // is entitled to the measured halves of a comparison that could not be
    // completed, as long as nothing pretends the comparison happened.
    let _ = writeln!(w, "\nbash re-run rate (the detector):");
    for arm in [cfetch, bare] {
        let _ = writeln!(
            w,
            "  {:<8} {:>7}  ({} repeat(s) of {} shell call(s) over {} session(s))",
            arm.map_or("-", |a| a.arm),
            arm.and_then(|a| a.rerun_pct)
                .map_or_else(|| "-".to_string(), |p| format!("{}%", num(p))),
            arm.map_or(0, |a| a.shell_repeats),
            arm.map_or(0, |a| a.shell_calls),
            arm.map_or(0, |a| a.rerun_sessions),
        );
    }
    match &r.rerun {
        Rerun::NotMeasured { reason } => {
            let _ = writeln!(w, "  COULD NOT MEASURE — {reason}");
        }
        Rerun::Tie { points_apart, .. } => {
            let _ = writeln!(
                w,
                "  TIE — {} point(s) apart, inside the {TIE_POINTS}-point band: the cfetch arm re-ran as many commands as the bare arm, so condensation destroyed information the agent then had to fetch again",
                num(*points_apart)
            );
        }
        Rerun::Lower { points_apart, .. } => {
            let _ = writeln!(
                w,
                "  LOWER by {} point(s) — the cfetch arm re-fetched less than the bare arm",
                num(*points_apart)
            );
        }
        Rerun::Higher { points_apart, .. } => {
            let _ = writeln!(
                w,
                "  HIGHER by {} point(s) — the cfetch arm re-fetched MORE than the bare arm",
                num(*points_apart)
            );
        }
    }

    let _ = writeln!(w, "\nmeasurement gaps:");
    if r.gaps.is_empty() {
        let _ = writeln!(w, "  none — every transcript in the window was classified");
    }
    for gap in &r.gaps {
        let _ = writeln!(w, "  {gap}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One assistant record = one API call, with the counters cfetch reads.
    fn assistant(id: &str, input: u64, output: u64, cache_read: u64, cache_created: u64) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"id":"{id}","model":"claude-test","content":[{{"type":"text","text":"ok"}}],"usage":{{"input_tokens":{input},"output_tokens":{output},"cache_read_input_tokens":{cache_read},"cache_creation_input_tokens":{cache_created}}}}},"uuid":"u-{id}"}}"#
        )
    }

    fn bash(id: &str, command: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"id":"{id}","model":"claude-test","content":[{{"type":"tool_use","id":"t-{id}","name":"Bash","input":{{"command":"{command}"}}}}],"usage":{{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}},"uuid":"u-{id}"}}"#
        )
    }

    fn prompt(text: &str) -> String {
        format!(r#"{{"type":"user","message":{{"role":"user","content":"{text}"}}}}"#)
    }

    /// The record shape `transcript::verified_injections` counts as a
    /// delivered injection.
    fn injection() -> String {
        r#"{"type":"hook_additional_context","content":"[cfetch] ring-0 invariants"}"#.to_string()
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        n: usize,
    }

    impl Fixture {
        fn new() -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join(".claude/projects/proj");
            std::fs::create_dir_all(&root).unwrap();
            Fixture { _dir: dir, root, n: 0 }
        }

        fn write(&mut self, lines: &[String]) {
            self.n += 1;
            let path = self.root.join(format!("s{}.jsonl", self.n));
            std::fs::write(path, lines.join("\n")).unwrap();
        }

        /// A session in one arm: `task` fixes the pair key, `commands` drive
        /// the re-run rate, the usage counters are the same in both arms
        /// unless a test says otherwise.
        fn session(&mut self, arm: &str, task: &str, commands: &[&str], cache_read: u64) {
            let mut lines = Vec::new();
            if arm == ARM_CFETCH {
                lines.push(injection());
            }
            lines.push(prompt(task));
            lines.push(assistant(&format!("m{}-a", self.n + 1), 100, 200, cache_read, 50));
            for (i, command) in commands.iter().enumerate() {
                lines.push(bash(&format!("m{}-b{i}", self.n + 1), command));
            }
            self.write(&lines);
        }

        fn build(&self) -> Report {
            build(
                &BenchPaths {
                    roots: vec![(agent_session::AGENT_CLAUDE, self.root.parent().unwrap().into())],
                },
                DEFAULT_WINDOW_DAYS,
                SystemTime::now(),
            )
        }
    }

    const SIX: [&str; 6] = ["ls", "cargo test", "git status", "cat a.rs", "cat b.rs", "grep x"];
    /// Same six calls, but three of them repeat: 50% re-run rate.
    const SIX_HALF_REPEATED: [&str; 6] = ["ls", "cargo test", "git status", "ls", "cargo test", "git status"];

    #[test]
    fn rerun_rate_counts_repeats_not_distinct_commands() {
        let mut f = Fixture::new();
        // Distinct commands only: nothing was re-fetched.
        f.session(ARM_CFETCH, "t1", &SIX, 1_000);
        f.session(ARM_CFETCH, "t2", &SIX, 1_000);
        f.session(ARM_CFETCH, "t3", &SIX, 1_000);
        // Half the calls repeat an earlier one.
        f.session(ARM_BARE, "t1", &SIX_HALF_REPEATED, 1_000);
        f.session(ARM_BARE, "t2", &SIX_HALF_REPEATED, 1_000);
        f.session(ARM_BARE, "t3", &SIX_HALF_REPEATED, 1_000);
        let r = f.build();
        let arm = |name| r.arms.iter().find(|a| a.arm == name).unwrap();
        assert_eq!(arm(ARM_CFETCH).rerun_pct, Some(0.0), "no repeats, no rate");
        assert_eq!(arm(ARM_BARE).rerun_pct, Some(50.0), "9 of 18 pooled calls repeated");
    }

    #[test]
    fn a_tie_within_two_points_reads_as_condensation_loss() {
        let mut f = Fixture::new();
        for task in ["t1", "t2", "t3"] {
            f.session(ARM_CFETCH, task, &SIX_HALF_REPEATED, 1_000);
            f.session(ARM_BARE, task, &SIX_HALF_REPEATED, 1_000);
        }
        let r = f.build();
        assert_eq!(
            r.rerun,
            Rerun::Tie { cfetch_pct: 50.0, bare_pct: 50.0, points_apart: 0.0 },
            "identical re-run rates: cfetch bought nothing"
        );
        let rendered = render(&r);
        assert!(rendered.contains("TIE"), "the verdict is stated, not left to the reader");
        assert!(
            rendered.contains("condensation destroyed information"),
            "the tie must name what it means"
        );
    }

    #[test]
    fn two_arms_that_never_repeated_a_command_are_not_a_tie() {
        let mut f = Fixture::new();
        for task in ["t1", "t2", "t3"] {
            f.session(ARM_CFETCH, task, &SIX, 1_000);
            f.session(ARM_BARE, task, &SIX, 1_000);
        }
        let r = f.build();
        let Rerun::NotMeasured { reason } = &r.rerun else {
            panic!("0% against 0% is an absent phenomenon, not a tie: {:?}", r.rerun);
        };
        assert!(reason.contains("band is wider than the phenomenon"), "{reason}");
        assert!(
            !render(&r).contains("condensation destroyed information"),
            "nothing was re-fetched, so condensation cannot be convicted of causing it"
        );
    }

    #[test]
    fn a_control_arm_inside_the_band_yields_no_verdict_either_way() {
        // The bare arm repeats 1 of 60 calls (1.7%) and the cfetch arm none
        // — cfetch removed EVERY re-fetch, yet the arms are 1.7 points
        // apart. Reading that as a tie would convict condensation of the
        // exact opposite of what happened. Measured on real sessions the
        // bare rate is ~3%, so this is the common case, not a corner.
        let distinct: Vec<String> = (0..60).map(|i| format!("cmd{i}")).collect();
        let mut repeated = distinct.clone();
        repeated[59] = distinct[0].clone();
        fn refs(v: &[String]) -> Vec<&str> {
            v.iter().map(String::as_str).collect()
        }
        let mut f = Fixture::new();
        for task in ["t1", "t2", "t3"] {
            f.session(ARM_CFETCH, task, &refs(&distinct), 1_000);
            f.session(ARM_BARE, task, &refs(&repeated), 1_000);
        }
        let r = f.build();
        let bare = r.arms.iter().find(|a| a.arm == ARM_BARE).unwrap();
        assert_eq!(bare.shell_repeats, 3, "one repeat in each of three sessions");
        assert_eq!(bare.shell_calls, 180);
        let Rerun::NotMeasured { reason } = &r.rerun else {
            panic!("a control arm inside the band supports no verdict: {:?}", r.rerun);
        };
        assert!(reason.contains("1.7%"), "the refusal quotes the rate it could not resolve: {reason}");
    }

    #[test]
    fn a_lower_cfetch_arm_is_not_reported_as_a_tie() {
        let mut f = Fixture::new();
        for task in ["t1", "t2", "t3"] {
            f.session(ARM_CFETCH, task, &SIX, 1_000);
            f.session(ARM_BARE, task, &SIX_HALF_REPEATED, 1_000);
        }
        let r = f.build();
        assert_eq!(
            r.rerun,
            Rerun::Lower { cfetch_pct: 0.0, bare_pct: 50.0, points_apart: 50.0 },
            "50 points apart is not a tie"
        );
    }

    #[test]
    fn a_higher_cfetch_arm_is_named_as_worse_not_folded_into_the_tie() {
        let mut f = Fixture::new();
        for task in ["t1", "t2", "t3"] {
            f.session(ARM_CFETCH, task, &SIX_HALF_REPEATED, 1_000);
            f.session(ARM_BARE, task, &SIX, 1_000);
        }
        let r = f.build();
        assert_eq!(
            r.rerun,
            Rerun::Higher { cfetch_pct: 50.0, bare_pct: 0.0, points_apart: 50.0 },
            "cfetch re-fetching more must not render as a win or a tie"
        );
        assert!(render(&r).contains("re-fetched MORE"));
    }

    #[test]
    fn too_few_sessions_refuse_a_verdict_instead_of_reporting_zero() {
        let mut f = Fixture::new();
        f.session(ARM_CFETCH, "t1", &SIX_HALF_REPEATED, 1_000);
        f.session(ARM_BARE, "t1", &SIX_HALF_REPEATED, 1_000);
        let r = f.build();
        assert!(
            matches!(r.rerun, Rerun::NotMeasured { .. }),
            "one session per arm is anecdote, not a measured tie"
        );
        assert_eq!(r.pairs_found, 1);
        assert!(r.paired.is_none(), "one pair states no difference");
        let rendered = render(&r);
        assert!(rendered.contains("paired deltas: COULD NOT MEASURE"));
        assert!(rendered.contains("1 pair(s) found"));
        // The measured halves survive the refusal: the reader sees both
        // rates and that they were not compared, not a blank.
        assert!(rendered.contains("cfetch       50%  (3 repeat(s) of 6 shell call(s)"), "{rendered}");
        assert!(rendered.contains("bare         50%  (3 repeat(s) of 6 shell call(s)"), "{rendered}");
    }

    #[test]
    fn a_short_session_carries_no_rate_at_all() {
        let mut f = Fixture::new();
        // Four calls is under the floor, however many of them repeat.
        f.session(ARM_CFETCH, "t1", &["ls", "ls", "ls", "ls"], 1_000);
        f.session(ARM_BARE, "t1", &["ls", "ls", "ls", "ls"], 1_000);
        let r = f.build();
        for arm in &r.arms {
            assert_eq!(arm.sessions, 1, "the session still counts for cost");
            assert_eq!(arm.rerun_sessions, 0, "but not for the detector");
            assert_eq!(arm.rerun_pct, None, "and contributes no calls to the pooled rate");
        }
    }

    #[test]
    fn paired_deltas_difference_the_arms_per_task() {
        let mut f = Fixture::new();
        // Same task in both arms, cfetch arm reading 400 fewer cache tokens.
        for (task, bare_cache) in [("t1", 1_000), ("t2", 2_000), ("t3", 9_000)] {
            f.session(ARM_CFETCH, task, &SIX, bare_cache - 400);
            f.session(ARM_BARE, task, &SIX, bare_cache);
        }
        let r = f.build();
        let paired = r.paired.expect("three pairs is enough to state a difference");
        assert_eq!(paired.pairs, 3);
        let delta = |name| paired.medians.iter().find(|d| d.dimension == name).unwrap().value;
        assert_eq!(delta("cache_read_tokens"), Some(-400.0), "median of the per-pair differences");
        assert_eq!(delta("api_calls"), Some(0.0), "same call count in both arms");
        // The unpaired medians are dominated by the task, which is exactly
        // why the paired figure exists: bare's median cache read is 2000.
        let bare = r.arms.iter().find(|a| a.arm == ARM_BARE).unwrap();
        let bare_cache = bare.medians.iter().find(|d| d.dimension == "cache_read_tokens").unwrap();
        assert_eq!(bare_cache.value, Some(2_000.0));
    }

    #[test]
    fn an_unpairable_task_is_not_paired_with_a_different_one() {
        let mut f = Fixture::new();
        for task in ["t1", "t2", "t3"] {
            f.session(ARM_CFETCH, task, &SIX, 1_000);
        }
        for task in ["t4", "t5", "t6"] {
            f.session(ARM_BARE, task, &SIX, 1_000);
        }
        let r = f.build();
        assert_eq!(r.pairs_found, 0, "different prompts are different tasks, never a pair");
        assert!(r.paired.is_none());
    }

    #[test]
    fn a_session_of_unknown_arm_becomes_a_gap_not_a_bare_run() {
        let mut f = Fixture::new();
        // The agent typed our name; nothing was ever injected. Counting this
        // as the bare arm would put a cfetch-flavored session in the control
        // group; counting it as the cfetch arm would credit us with context
        // we never delivered.
        f.write(&[
            prompt("t1"),
            bash("m1-b0", "cfetch recall zfs"),
            assistant("m1-a", 100, 200, 1_000, 50),
        ]);
        f.session(ARM_BARE, "t1", &SIX, 1_000);
        let r = f.build();
        assert_eq!(r.scanned, 2);
        let arm = |name| r.arms.iter().find(|a| a.arm == name).unwrap();
        assert_eq!(arm(ARM_BARE).sessions, 1, "only the genuinely bare run is in the bare arm");
        assert_eq!(arm(ARM_CFETCH).sessions, 0);
        assert_eq!(r.gaps.len(), 1);
        assert!(r.gaps[0].contains("no injection could be verified"), "{:?}", r.gaps);
        assert!(render(&r).contains("no injection could be verified"));
    }

    #[test]
    fn identical_means_identical_not_the_same_shape_of_command() {
        // The buglog normalizer used by the promotion traps folds both of
        // these to `cat <path>`; reading two different files is not a
        // re-fetch, and scoring it as one would invent a re-run rate.
        assert_ne!(command_key("cat src/a.rs"), command_key("cat src/b.rs"));
        assert_eq!(command_key("cargo  test\n"), command_key("cargo test"));
    }

    #[test]
    fn an_arm_with_no_sessions_is_a_dash_never_a_zero() {
        let mut f = Fixture::new();
        f.session(ARM_BARE, "t1", &SIX, 1_000);
        let r = f.build();
        let cfetch = r.arms.iter().find(|a| a.arm == ARM_CFETCH).unwrap();
        assert_eq!(cfetch.sessions, 0);
        assert!(
            cfetch.medians.iter().all(|d| d.value.is_none()),
            "an arm nobody ran has no median, and a zero would read as one"
        );
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("{\"dimension\":\"api_calls\",\"value\":null}"), "{json}");
        let line = render(&r)
            .lines()
            .find(|l| l.trim_start().starts_with("api_calls"))
            .unwrap()
            .to_string();
        assert!(line.contains('-'), "empty arm renders as a dash: {line}");
    }

    #[test]
    fn the_method_travels_with_the_numbers() {
        let r = Fixture::new().build();
        let rendered = render(&r);
        for line in &r.method {
            assert!(rendered.contains(line), "method line missing from the rendering: {line}");
        }
        assert!(rendered.contains("COULD NOT MEASURE"), "an empty bench measures nothing");
    }
}
