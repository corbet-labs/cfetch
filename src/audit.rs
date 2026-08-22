//! Context audit: the always-on context bill priced in one report.
//!
//! Every session pays for CLAUDE.md, its @-imports, every MCP server's tool
//! schemas, and every byte cfetch itself injects — before the first word of
//! work. This module prices that bill. Static file costs are ESTIMATED
//! (chars/3.5, labeled as such); injection costs come from the ledger's
//! booked counters; and anything that could not be measured is reported as an
//! explicit measurement gap — an audit that silently omits what it failed to
//! see would be advertising, not auditing.
//!
//! Position-weighted cost (the dossier formula): bytes injected at call 0 of
//! an n-call session are re-read by every later call, so their effective
//! cost is ~n times their size in cache reads. `cost_weight` encodes that;
//! raw token counts mis-rank waste.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::paths;

/// Ledger sessions older than this are outside the audit window.
pub const WINDOW_DAYS: u64 = 14;
/// CLAUDE.md above this line count draws a warning.
pub const CLAUDE_MD_WARN_LINES: usize = 200;
/// The ledger source label the resident digest is booked under (hooks.rs).
const RESIDENT_SOURCE: &str = "resident-digest";

/// Every path the audit reads, overridable so tests run against a fabricated
/// home directory instead of the operator's real one.
pub struct AuditPaths {
    pub claude_md: PathBuf,
    pub settings_json: PathBuf,
    pub mcp_json: PathBuf,
    /// Resolves `@~/` imports.
    pub home: PathBuf,
    /// Where session transcripts live (native layout: one dir per project).
    pub transcripts_root: PathBuf,
    /// Codex's date-nested session transcripts.
    pub codex_transcripts_root: PathBuf,
}

impl AuditPaths {
    pub fn defaults() -> AuditPaths {
        let home = paths::home();
        AuditPaths {
            claude_md: home.join(".claude/CLAUDE.md"),
            settings_json: home.join(".claude/settings.json"),
            mcp_json: std::env::current_dir()
                .map(|d| d.join(".mcp.json"))
                .unwrap_or_else(|_| PathBuf::from(".mcp.json")),
            home,
            transcripts_root: paths::native_projects_root(),
            codex_transcripts_root: paths::codex_sessions_root(),
        }
    }
}

/// One `@`-import found in CLAUDE.md, priced. `None` costs mean the imported
/// file could not be read — reported, never dropped.
#[derive(Debug, Serialize)]
pub struct ImportCost {
    pub spec: String,
    pub path: String,
    pub lines: Option<usize>,
    pub chars: Option<u64>,
    pub tokens_estimated: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct ClaudeMdCost {
    pub lines: usize,
    pub chars: u64,
    pub tokens_estimated: u64,
    /// True above CLAUDE_MD_WARN_LINES.
    pub over_line_warning: bool,
    pub imports: Vec<ImportCost>,
}

/// MCP servers found in one config file. Schema cost is presence-only: the
/// harness serializes each server's tool schemas into every session, but
/// their size is not measurable from here.
#[derive(Debug, Serialize)]
pub struct McpFile {
    pub source: String,
    pub present: bool,
    pub parse_ok: bool,
    pub servers: Vec<String>,
}

/// Booked injections for one source across the window.
#[derive(Debug, Serialize)]
pub struct SourceCost {
    pub source: String,
    pub sessions: usize,
    pub injections: u64,
    pub chars: u64,
    pub tokens_estimated: u64,
    /// Largest single-session chars — the budget is per session, so the
    /// anomaly signal is the per-session peak, not the window total.
    pub max_session_chars: u64,
    /// True when max_session_chars exceeds 2x budget_chars.
    pub over_budget_warning: bool,
}

/// The resident digest's estimated recurring cost per session: injected at
/// call 0, it is re-read by every later call (cost_weight at index 0).
#[derive(Debug, Serialize)]
pub struct RecurringCost {
    pub digest_tokens_estimated: u64,
    pub api_calls_per_session: u64,
    pub recurring_tokens_estimated: u64,
}

#[derive(Debug, Serialize)]
pub struct AuditReport {
    pub window_days: u64,
    pub budget_chars: usize,
    pub claude_md_path: String,
    /// `None` = CLAUDE.md not found (rendered explicitly, never omitted).
    pub claude_md: Option<ClaudeMdCost>,
    pub mcp: Vec<McpFile>,
    pub sources: Vec<SourceCost>,
    pub measured_sessions: usize,
    pub measured_api_calls: u64,
    pub recurring: Option<RecurringCost>,
    /// Explicit measurement gaps — what this audit could NOT see.
    pub gaps: Vec<String>,
}

/// Position-weighted cost of injected tokens (the dossier formula): bytes
/// entering at `call_index` of a `total_calls`-call session are re-read by
/// every later call, so their effective cost is `tokens * (total_calls -
/// call_index)` in cache reads. Index 0 costs ~total_calls times the size;
/// an index at or past the end clamps to zero.
pub fn cost_weight(tokens: u64, call_index: u64, total_calls: u64) -> u64 {
    tokens.saturating_mul(total_calls.saturating_sub(call_index))
}

/// The import spec on one CLAUDE.md line: a line-leading `@` (the harness's
/// import syntax) or an embedded `@~/` reference. First whitespace-delimited
/// token, `@` stripped. Anything else is prose, not an import.
fn import_spec(line: &str) -> Option<String> {
    let t = line.trim();
    if let Some(rest) = t.strip_prefix('@') {
        return rest.split_whitespace().next().map(String::from);
    }
    if let Some(pos) = t.find("@~/") {
        return t[pos + 1..].split_whitespace().next().map(String::from);
    }
    None
}

fn resolve_import(spec: &str, home: &Path, claude_md_dir: &Path) -> PathBuf {
    if let Some(rest) = spec.strip_prefix("~/") {
        home.join(rest)
    } else if Path::new(spec).is_absolute() {
        PathBuf::from(spec)
    } else {
        claude_md_dir.join(spec)
    }
}

/// Prices CLAUDE.md and every import it pulls in. `None` = file not found.
fn claude_md_cost(claude_md: &Path, home: &Path) -> Option<ClaudeMdCost> {
    let text = std::fs::read_to_string(claude_md).ok()?;
    let dir = claude_md.parent().unwrap_or(Path::new("/"));
    let mut imports = Vec::new();
    for line in text.lines() {
        let Some(spec) = import_spec(line) else { continue };
        let resolved = resolve_import(&spec, home, dir);
        let body = std::fs::read_to_string(&resolved).ok();
        imports.push(ImportCost {
            spec,
            path: resolved.display().to_string(),
            lines: body.as_ref().map(|b| b.lines().count()),
            chars: body.as_ref().map(|b| b.len() as u64),
            tokens_estimated: body.as_ref().map(|b| crate::hook_io::estimate_tokens(b.len())),
        });
    }
    let lines = text.lines().count();
    Some(ClaudeMdCost {
        lines,
        chars: text.len() as u64,
        tokens_estimated: crate::hook_io::estimate_tokens(text.len()),
        over_line_warning: lines > CLAUDE_MD_WARN_LINES,
        imports,
    })
}

/// Lists MCP server names from one config file's `mcpServers` object.
fn mcp_file(path: &Path) -> McpFile {
    let source = path.display().to_string();
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return McpFile { source, present: false, parse_ok: false, servers: vec![] },
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return McpFile { source, present: true, parse_ok: false, servers: vec![] };
    };
    let servers = v
        .get("mcpServers")
        .and_then(|m| m.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    McpFile { source, present: true, parse_ok: true, servers }
}

#[derive(Default)]
struct SourceAgg {
    sessions: usize,
    injections: u64,
    chars: u64,
    tokens_estimated: u64,
    max_session_chars: u64,
}

/// Builds the report. The `ledger` is the DERIVED fleet view folded from the
/// tree's ledger streams (see [`crate::ledger::read`]) — the audit prices what
/// it is handed and never reaches for a store of its own. `now` is a parameter
/// so window math is testable.
pub fn build(
    paths: &AuditPaths,
    ledger: &crate::ledger::Ledger,
    budget_chars: usize,
    now: u64,
) -> AuditReport {
    let claude_md = claude_md_cost(&paths.claude_md, &paths.home);
    let mcp = vec![mcp_file(&paths.settings_json), mcp_file(&paths.mcp_json)];

    // Booked injections + measured usage, windowed. The budget is per
    // session, so the warning keys on the per-session peak per source.
    let cutoff = now.saturating_sub(WINDOW_DAYS * 86_400);
    let mut per_source: BTreeMap<String, SourceAgg> = BTreeMap::new();
    let mut measured_sessions = 0usize;
    let mut measured_api_calls = 0u64;
    let mut digest_sessions = 0usize;
    let mut digest_tokens = 0u64;
    for s in ledger.sessions.values() {
        if s.started_at < cutoff {
            continue;
        }
        for (name, t) in &s.by_source {
            let agg = per_source.entry(name.clone()).or_default();
            agg.sessions += 1;
            agg.injections += t.count;
            agg.chars += t.chars;
            agg.tokens_estimated += t.tokens_estimated;
            agg.max_session_chars = agg.max_session_chars.max(t.chars);
        }
        if !s.measured.is_zero() {
            measured_sessions += 1;
            measured_api_calls += s.measured.api_calls;
        }
        if let Some(d) = s.by_source.get(RESIDENT_SOURCE) {
            digest_sessions += 1;
            digest_tokens += d.tokens_estimated;
        }
    }
    let sources: Vec<SourceCost> = per_source
        .into_iter()
        .map(|(source, a)| SourceCost {
            source,
            sessions: a.sessions,
            injections: a.injections,
            chars: a.chars,
            tokens_estimated: a.tokens_estimated,
            max_session_chars: a.max_session_chars,
            over_budget_warning: a.max_session_chars > 2 * budget_chars as u64,
        })
        .collect();

    // Recurring digest cost needs BOTH halves: a booked digest size and a
    // measured call count. Anything less would be an invented number.
    let recurring = if digest_sessions > 0 && measured_sessions > 0 {
        let digest_avg = digest_tokens / digest_sessions as u64;
        let calls_avg = measured_api_calls / measured_sessions as u64;
        Some(RecurringCost {
            digest_tokens_estimated: digest_avg,
            api_calls_per_session: calls_avg,
            recurring_tokens_estimated: cost_weight(digest_avg, 0, calls_avg),
        })
    } else {
        None
    };

    let mut gaps = Vec::new();
    if crate::transcript::newest_transcript_among(&[
        paths.transcripts_root.clone(),
        paths.codex_transcripts_root.clone(),
    ])
    .is_none()
    {
        gaps.push(format!(
            "no transcripts found under {} or {} — delivery and usage cannot be verified",
            paths.transcripts_root.display(),
            paths.codex_transcripts_root.display()
        ));
    }
    if measured_sessions == 0 {
        gaps.push(format!(
            "no measured usage booked in the last {WINDOW_DAYS} days — every token figure here is a chars/3.5 estimate"
        ));
    }

    AuditReport {
        window_days: WINDOW_DAYS,
        budget_chars,
        claude_md_path: paths.claude_md.display().to_string(),
        claude_md,
        mcp,
        sources,
        measured_sessions,
        measured_api_calls,
        recurring,
        gaps,
    }
}

/// Renders the report as terminal text.
pub fn render(r: &AuditReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let w = &mut out;
    let _ = writeln!(w, "context audit — the always-on bill (window: last {} days)", r.window_days);

    let _ = writeln!(w, "\nCLAUDE.md ({}):", r.claude_md_path);
    match &r.claude_md {
        None => {
            let _ = writeln!(w, "  not found — nothing injected from it");
        }
        Some(md) => {
            let _ = writeln!(
                w,
                "  {} lines, {} chars, ~{} tokens (estimated)",
                md.lines, md.chars, md.tokens_estimated
            );
            if md.over_line_warning {
                let _ = writeln!(
                    w,
                    "  WARN over {CLAUDE_MD_WARN_LINES} lines — every line rides into every session"
                );
            }
            for i in &md.imports {
                match (i.lines, i.tokens_estimated) {
                    (Some(lines), Some(tok)) => {
                        let _ = writeln!(
                            w,
                            "  import @{} -> {}: {} lines, ~{} tokens (estimated)",
                            i.spec, i.path, lines, tok
                        );
                    }
                    _ => {
                        let _ = writeln!(
                            w,
                            "  import @{} -> {}: UNREADABLE — imported but not priceable",
                            i.spec, i.path
                        );
                    }
                }
            }
        }
    }

    let _ = writeln!(w, "\nmcp servers (schema cost rides into every session; size not measured here):");
    for m in &r.mcp {
        if !m.present {
            let _ = writeln!(w, "  {}: not present", m.source);
        } else if !m.parse_ok {
            let _ = writeln!(w, "  {}: unparseable — schema cost unknown", m.source);
        } else if m.servers.is_empty() {
            let _ = writeln!(w, "  {}: no MCP servers configured", m.source);
        } else {
            let _ = writeln!(w, "  {}: {}", m.source, m.servers.join(", "));
        }
    }

    let _ = writeln!(w, "\ninjections by source (ledger, last {} days):", r.window_days);
    if r.sources.is_empty() {
        let _ = writeln!(w, "  no injections booked in the window");
    }
    for s in &r.sources {
        let _ = writeln!(
            w,
            "  {}: {} injection(s) across {} session(s), {} chars, ~{} tokens (estimated)",
            s.source, s.injections, s.sessions, s.chars, s.tokens_estimated
        );
        if s.over_budget_warning {
            let _ = writeln!(
                w,
                "    WARN peaked at {} chars in one session — over 2x the {}-char budget",
                s.max_session_chars, r.budget_chars
            );
        }
    }

    let _ = writeln!(w, "\nposition-weighted cost — cost(tokens, i) = tokens x (total_calls - i):");
    let _ = writeln!(
        w,
        "  early-session injected bytes are re-read by every later api call, so they cost ~total_calls times their size in cache reads"
    );
    match &r.recurring {
        Some(rec) => {
            let _ = writeln!(
                w,
                "  resident digest recurring cost: ~{} tokens x ~{} api calls/session = ~{} tokens/session (estimated)",
                rec.digest_tokens_estimated, rec.api_calls_per_session, rec.recurring_tokens_estimated
            );
        }
        None => {
            let _ = writeln!(
                w,
                "  resident digest recurring cost: unavailable — needs both booked digest injections and measured api calls"
            );
        }
    }

    let _ = writeln!(w, "\nmeasurement gaps:");
    if r.gaps.is_empty() {
        let _ = writeln!(w, "  none — transcripts and measured usage both present");
    }
    for g in &r.gaps {
        let _ = writeln!(w, "  measurement gap: {g}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{Ledger, MeasuredUsage, SessionInjections, SourceTotals};

    /// A fabricated home: CLAUDE.md with two imports (one inline `@~/`, one
    /// leading-`@` relative), settings.json with two MCP servers, a project
    /// .mcp.json with one, an empty transcripts root, and a ledger dir.
    fn fab(dir: &Path) -> AuditPaths {
        let claude = dir.join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        // 18 chars -> ceil(18/3.5) = 6 estimated tokens.
        std::fs::write(dir.join("brain.md"), "line one\nline two\n").unwrap();
        // 8 chars -> 3 estimated tokens.
        std::fs::write(claude.join("extra.md"), "abcdefg\n").unwrap();
        std::fs::write(
            claude.join("CLAUDE.md"),
            "# Instructions\n\nSee @~/brain.md for the brain.\n@extra.md\nplain line\n",
        )
        .unwrap();
        std::fs::write(
            claude.join("settings.json"),
            r#"{"mcpServers": {"github": {"command": "x"}, "filesystem": {"command": "y"}}}"#,
        )
        .unwrap();
        std::fs::write(dir.join(".mcp.json"), r#"{"mcpServers": {"project-db": {}}}"#).unwrap();
        let transcripts = dir.join("projects");
        std::fs::create_dir_all(&transcripts).unwrap();
        AuditPaths {
            claude_md: claude.join("CLAUDE.md"),
            settings_json: claude.join("settings.json"),
            mcp_json: dir.join(".mcp.json"),
            home: dir.to_path_buf(),
            transcripts_root: transcripts,
            codex_transcripts_root: dir.join("codex-sessions"),
        }
    }

    fn session(
        started_at: u64,
        sources: &[(&str, u64, u64, u64)], // (name, count, chars, tokens)
        measured: MeasuredUsage,
    ) -> SessionInjections {
        let mut by_source = std::collections::BTreeMap::new();
        for (name, count, chars, tokens) in sources {
            by_source.insert(
                name.to_string(),
                SourceTotals { count: *count, chars: *chars, tokens_estimated: *tokens },
            );
        }
        SessionInjections { started_at, by_source, measured, booked: MeasuredUsage::default() }
    }

    const NOW: u64 = 100_000_000;

    #[test]
    fn cost_weight_matches_the_dossier_formula() {
        assert_eq!(cost_weight(100, 0, 50), 5000, "call-0 bytes are re-read ~total_calls times");
        assert_eq!(cost_weight(100, 49, 50), 100, "last-call bytes cost themselves once");
        assert_eq!(cost_weight(100, 50, 50), 0);
        assert_eq!(cost_weight(100, 60, 50), 0, "index past the end clamps, never underflows");
        assert_eq!(cost_weight(0, 0, 50), 0);
    }

    #[test]
    fn fabricated_home_prices_claude_md_imports_and_mcp() {
        let dir = tempfile::tempdir().unwrap();
        let p = fab(dir.path());
        let r = build(&p, &Ledger::default(), 6000, NOW);

        let md = r.claude_md.as_ref().expect("CLAUDE.md exists in the fabricated home");
        assert_eq!(md.lines, 5);
        assert!(!md.over_line_warning);
        assert_eq!(md.imports.len(), 2, "one inline @~/ import + one leading-@ import");
        let inline = &md.imports[0];
        assert_eq!(inline.spec, "~/brain.md");
        assert!(inline.path.ends_with("brain.md"));
        assert_eq!(inline.lines, Some(2));
        assert_eq!(inline.chars, Some(18));
        assert_eq!(inline.tokens_estimated, Some(6), "chars/3.5, ceiled");
        let leading = &md.imports[1];
        assert_eq!(leading.spec, "extra.md");
        // Component-wise: the resolved import is an OS-native path, so a
        // `/`-joined literal would fail on Windows for a correct result.
        let want: Vec<_> = std::path::Path::new(".claude/extra.md").components().collect();
        let got: Vec<_> = std::path::Path::new(&leading.path).components().collect();
        assert!(
            got.len() >= want.len() && got[got.len() - want.len()..] == want[..],
            "relative to CLAUDE.md's dir, got {}", leading.path
        );
        assert_eq!(leading.tokens_estimated, Some(3));

        assert_eq!(r.mcp.len(), 2);
        let settings = &r.mcp[0];
        assert!(settings.present && settings.parse_ok);
        assert_eq!(settings.servers, vec!["filesystem".to_string(), "github".to_string()]);
        let mcp_json = &r.mcp[1];
        assert_eq!(mcp_json.servers, vec!["project-db".to_string()]);
    }

    #[test]
    fn claude_md_over_200_lines_warns() {
        let dir = tempfile::tempdir().unwrap();
        let p = fab(dir.path());
        std::fs::write(&p.claude_md, "x\n".repeat(CLAUDE_MD_WARN_LINES + 1)).unwrap();
        let r = build(&p, &Ledger::default(), 6000, NOW);
        assert!(r.claude_md.unwrap().over_line_warning);
        let r = build(&p, &Ledger::default(), 6000, NOW);
        assert!(render(&r).contains("WARN"), "the line warning must reach the rendered report");
    }

    #[test]
    fn missing_claude_md_is_reported_not_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let p = fab(dir.path());
        std::fs::remove_file(&p.claude_md).unwrap();
        let r = build(&p, &Ledger::default(), 6000, NOW);
        assert!(r.claude_md.is_none());
        assert!(render(&r).contains("not found"), "absence is a line, never silence");
        // Absent MCP files are labeled too.
        std::fs::remove_file(&p.mcp_json).unwrap();
        let r = build(&p, &Ledger::default(), 6000, NOW);
        assert!(!r.mcp[1].present);
        assert!(render(&r).contains("not present"));
    }

    #[test]
    fn source_over_2x_budget_in_a_single_session_warns() {
        let dir = tempfile::tempdir().unwrap();
        let p = fab(dir.path());
        let mut l = Ledger::default();
        l.sessions.insert(
            "s1".into(),
            session(NOW - 100, &[("resident-digest", 1, 13_000, 3715)], MeasuredUsage::default()),
        );
        l.sessions.insert(
            "s2".into(),
            session(NOW - 200, &[("read-advisory", 2, 400, 115)], MeasuredUsage::default()),
        );
        let r = build(&p, &l, 6000, NOW);
        let digest = r.sources.iter().find(|s| s.source == "resident-digest").unwrap();
        assert!(digest.over_budget_warning, "13000 chars > 2x 6000 budget");
        assert_eq!(digest.max_session_chars, 13_000);
        let advisory = r.sources.iter().find(|s| s.source == "read-advisory").unwrap();
        assert!(!advisory.over_budget_warning);
        assert!(render(&r).contains("WARN"));
    }

    #[test]
    fn out_of_window_sessions_are_excluded() {
        let dir = tempfile::tempdir().unwrap();
        let p = fab(dir.path());
        let mut l = Ledger::default();
        l.sessions.insert(
            "old".into(),
            session(
                NOW - (WINDOW_DAYS + 1) * 86_400,
                &[("resident-digest", 1, 100, 29)],
                MeasuredUsage { api_calls: 9, ..Default::default() },
            ),
        );
        l.sessions.insert(
            "fresh".into(),
            session(NOW - 3600, &[("read-advisory", 1, 50, 15)], MeasuredUsage::default()),
        );
        let r = build(&p, &l, 6000, NOW);
        assert_eq!(r.sources.len(), 1, "only the in-window session's sources appear");
        assert_eq!(r.sources[0].source, "read-advisory");
        assert_eq!(r.measured_api_calls, 0, "out-of-window measured usage is excluded too");
    }

    #[test]
    fn missing_transcripts_and_unmeasured_usage_are_explicit_gaps() {
        let dir = tempfile::tempdir().unwrap();
        let p = fab(dir.path());
        // Empty transcripts root, empty ledger: BOTH gaps must be named.
        let r = build(&p, &Ledger::default(), 6000, NOW);
        assert!(r.gaps.iter().any(|g| g.contains("no transcripts found")), "gaps: {:?}", r.gaps);
        assert!(r.gaps.iter().any(|g| g.contains("no measured usage")), "gaps: {:?}", r.gaps);
        let text = render(&r);
        assert!(text.contains("measurement gap"));
        assert!(text.contains("no transcripts found"));

        // With a transcript present and measured usage booked, the gaps close.
        let proj = p.transcripts_root.join("-home-x");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("s1.jsonl"), "{}").unwrap();
        let mut l = Ledger::default();
        l.sessions.insert(
            "s1".into(),
            session(
                NOW - 100,
                &[("resident-digest", 1, 700, 200)],
                MeasuredUsage { api_calls: 4, input_tokens: 10, ..Default::default() },
            ),
        );
        let r = build(&p, &l, 6000, NOW);
        assert!(r.gaps.is_empty(), "gaps must close when data exists: {:?}", r.gaps);
        assert!(render(&r).contains("none"));
    }

    #[test]
    fn recurring_digest_cost_is_digest_tokens_times_api_calls() {
        let dir = tempfile::tempdir().unwrap();
        let p = fab(dir.path());
        let mut l = Ledger::default();
        l.sessions.insert(
            "s1".into(),
            session(
                NOW - 100,
                &[("resident-digest", 1, 2800, 800)],
                MeasuredUsage { api_calls: 40, input_tokens: 5, ..Default::default() },
            ),
        );
        l.sessions.insert(
            "s2".into(),
            session(
                NOW - 200,
                &[("resident-digest", 1, 3500, 1000)],
                MeasuredUsage { api_calls: 20, input_tokens: 5, ..Default::default() },
            ),
        );
        let r = build(&p, &l, 6000, NOW);
        let rec = r.recurring.as_ref().expect("digest bookings + measured calls => recurring cost");
        assert_eq!(rec.digest_tokens_estimated, 900, "mean digest tokens per session");
        assert_eq!(rec.api_calls_per_session, 30, "mean api calls per measured session");
        assert_eq!(rec.recurring_tokens_estimated, 27_000, "digest_tokens * api_calls");
        assert!(render(&r).contains("27000"));

        // Without measured api calls the line cannot be computed — and says so.
        let mut l = Ledger::default();
        l.sessions.insert(
            "s1".into(),
            session(NOW - 100, &[("resident-digest", 1, 2800, 800)], MeasuredUsage::default()),
        );
        let r = build(&p, &l, 6000, NOW);
        assert!(r.recurring.is_none());
        assert!(render(&r).contains("unavailable"));
    }
}
