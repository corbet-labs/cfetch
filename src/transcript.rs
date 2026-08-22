//! Measured token usage from the harness session transcript (Milestone 5).
//!
//! The transcript (JSONL at `transcript_path`) is an INTERNAL file of the
//! agent harness, not an API — its schema is unstable and has drifted between
//! releases. So a schema probe runs before any extraction: attempt-parse the
//! first lines and require at least half of them to be JSON objects, otherwise
//! return `None` and let callers label their numbers "estimated". A
//! measured-looking zero from an unrecognized schema would be a lie (upstream
//! measured hook self-reports drifting ~20x from transcript ground truth).
//!
//! Streaming writes several records per API call and resume replays old
//! lines, so usage is deduped per message id, keeping the LAST record —
//! cumulative usage grows within one streamed message. Distinct ids are the
//! api-call count.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Usage summed over all distinct API calls found in one transcript. The
/// counters are cumulative for the SESSION — booking per-turn deltas out of
/// them is the ledger's job (see `ledger::book_measured`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TranscriptUsage {
    pub api_calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

/// How many leading non-empty lines the schema probe samples.
const PROBE_LINES: usize = 20;

/// The usage keys we know how to read. A "usage" object carrying none of
/// them is a different schema, not a zero.
const USAGE_KEYS: [&str; 4] =
    ["input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"];

/// Reads and sums measured usage. `None` means "could not measure" — file
/// unreadable, schema probe refused, or no recognizable usage records.
pub fn scan(path: &Path) -> Option<TranscriptUsage> {
    scan_text(&std::fs::read_to_string(path).ok()?)
}

/// Schema probe shared by every extractor: at least half of the first
/// PROBE_LINES non-empty lines must parse as JSON objects, or the file is
/// treated as unparseable (callers return `None`, never a measured zero).
fn probe_ok(lines: &[&str]) -> bool {
    let probe = &lines[..lines.len().min(PROBE_LINES)];
    let yielded = probe
        .iter()
        .filter(|l| serde_json::from_str::<serde_json::Value>(l).is_ok_and(|v| v.is_object()))
        .count();
    yielded * 2 >= probe.len()
}

fn scan_text(text: &str) -> Option<TranscriptUsage> {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() || !probe_ok(&lines) {
        return None;
    }

    // Last usage per message id wins; a line that fails to parse mid-file is
    // skipped (one torn write must not discard the rest).
    let mut per_id: BTreeMap<String, [u64; 4]> = BTreeMap::new();
    let mut codex_totals = Vec::new();
    for line in &lines {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if let Some((id, usage)) = usage_record(&v) {
            per_id.insert(id, usage);
        }
        if let Some(usage) = codex_usage_record(&v) {
            codex_totals.push(usage);
        }
    }
    // JSON parsed but zero usage records recognized: the schema drifted under
    // us. Refuse to report a measured zero.
    if per_id.is_empty() {
        let last = *codex_totals.last()?;
        let calls = codex_totals.into_iter().collect::<BTreeSet<_>>().len() as u64;
        return Some(TranscriptUsage {
            api_calls: calls,
            input_tokens: last[0],
            output_tokens: last[1],
            cache_read_input_tokens: last[2],
            cache_creation_input_tokens: last[3],
        });
    }

    let mut total = TranscriptUsage { api_calls: per_id.len() as u64, ..Default::default() };
    for [inp, out, cr, cc] in per_id.values() {
        total.input_tokens += inp;
        total.output_tokens += out;
        total.cache_read_input_tokens += cr;
        total.cache_creation_input_tokens += cc;
    }
    Some(total)
}

/// Transcript-VERIFIED hook delivery: `Some((fired, delivered))` where
/// `fired` counts records mentioning our hook command (`cfetch hook`) and
/// `delivered` counts how many of those carried a non-empty
/// `additionalContext` into the conversation. Ground truth for "did the
/// injection actually enter context" — hook self-reports drifted ~20x
/// upstream. `None` means "could not verify": file unreadable, schema probe
/// refused, or zero hook records recognized (a transcript whose hook records
/// we no longer recognize must read as unverifiable, never as zero).
pub fn verified_injections(path: &Path) -> Option<(u64, u64)> {
    verified_injections_text(&std::fs::read_to_string(path).ok()?)
}

fn verified_injections_text(text: &str) -> Option<(u64, u64)> {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() || !probe_ok(&lines) {
        return None;
    }
    let mut fired = 0u64;
    let mut delivered = 0u64;
    for line in &lines {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if is_user_record(&v) {
            continue;
        }
        // The harness writes exactly ONE attachment per firing and encodes the
        // outcome in its type, so a delivery record stands alone and never
        // names the command that produced it. Both kinds are therefore a
        // firing, and only one of them is also a delivery.
        let named = has_cfetch_hook_record(&v);
        let injected = has_cfetch_injection(&v);
        if !named && !injected {
            continue;
        }
        fired += 1;
        if injected || (named && has_nonempty_additional_context(&v)) {
            delivered += 1;
        }
    }
    // Zero recognized hook records: cannot distinguish "hooks never fired"
    // from "the record shape drifted" — unverifiable, not zero.
    if fired == 0 {
        return None;
    }
    Some((fired, delivered))
}

/// Every string cfetch injects begins with this. It is the ONLY link between a
/// standalone `hook_additional_context` record and us: that record carries the
/// content and the event name, never the command, and several tools can be
/// registered on one event. Injection sites must keep the prefix.
const INJECTION_SIGNATURE: &str = "[cfetch";

/// A delivery record carrying OUR injection. Deliberately narrow: it matches
/// the harness's structured content field, not a mention anywhere in a message.
fn has_cfetch_injection(v: &serde_json::Value) -> bool {
    fn ours(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::String(s) => s.trim_start().starts_with(INJECTION_SIGNATURE),
            serde_json::Value::Array(a) => a.iter().any(ours),
            _ => false,
        }
    }
    match v {
        serde_json::Value::Object(m) => {
            if m.get("type").and_then(serde_json::Value::as_str) == Some("hook_additional_context")
                && m.get("content").is_some_and(ours)
            {
                return true;
            }
            if m.get("hookAdditionalContext").is_some_and(ours) {
                return true;
            }
            m.values().any(has_cfetch_injection)
        }
        serde_json::Value::Array(a) => a.iter().any(has_cfetch_injection),
        _ => false,
    }
}

fn is_user_record(v: &serde_json::Value) -> bool {
    v.get("type").and_then(serde_json::Value::as_str) == Some("user")
        || v.pointer("/message/role").and_then(serde_json::Value::as_str) == Some("user")
        || v.get("role").and_then(serde_json::Value::as_str) == Some("user")
}

fn is_cfetch_hook_command(command: &str) -> bool {
    let words = crate::exhaust::shell_words(command);
    if words.len() < 3 || words.get(1).map(String::as_str) != Some("hook") {
        return false;
    }
    Path::new(&words[0])
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "cfetch" || name == "cfetch.exe")
}

/// Recognizes only harness hook fields, never a raw prose mention or an
/// agent's own shell command. The shapes below were captured from real Claude
/// Code 2.1.233 transcripts; the two legacy spellings are kept because a
/// harness that emits them must not read as unverifiable.
///
/// - `attachment: { type: "hook_success" | "hook_cancelled", command }` — the
///   per-hook record. A cancelled hook FIRED; it simply delivered nothing,
///   which is the distinction that makes a timing-out hook visible at all.
/// - `hookInfos: [ { command } ]` — inside `stop_hook_summary`, which is where
///   a Stop hook is actually recorded.
/// - `hookCommand`, `hook: { command }` — tolerated spellings.
///
/// The container is always part of the match. A Bash tool_use record also
/// carries a bare `command`, so keying on that alone would count an agent
/// typing `cfetch` by hand as a hook firing.
fn has_cfetch_hook_record(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Object(map) => {
            let is_hook_attachment = map
                .get("type")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|t| t == "hook_success" || t == "hook_cancelled");
            if is_hook_attachment
                && map
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(is_cfetch_hook_command)
            {
                return true;
            }
            if map
                .get("hookInfos")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|infos| {
                    infos.iter().any(|i| {
                        i.get("command")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(is_cfetch_hook_command)
                    })
                })
            {
                return true;
            }
            map.iter().any(|(key, value)| {
                (key == "hookCommand" && value.as_str().is_some_and(is_cfetch_hook_command))
                    || (key == "hook"
                        && value
                            .get("command")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(is_cfetch_hook_command))
                    || has_cfetch_hook_record(value)
            })
        }
        serde_json::Value::Array(values) => values.iter().any(has_cfetch_hook_record),
        _ => false,
    }
}

/// Did this record carry injected context INTO the conversation? Real shapes,
/// captured from Claude Code 2.1.233:
///
/// - `attachment: { type: "hook_additional_context", content: [..] }` — the
///   delivery record proper. `content` is an array of injected strings.
/// - `hookAdditionalContext: [..]` on `stop_hook_summary` — empty array means
///   the hooks ran and injected nothing.
/// - `additionalContext` / `additional_context` as a plain string — the
///   documented spelling, kept because the nesting has drifted across versions.
///
/// A `hook_cancelled` record matches none of these, which is the point: a hook
/// that timed out is counted as fired and NOT delivered, so the gap between the
/// two numbers is exactly the breakage the health check exists to surface.
fn has_nonempty_additional_context(v: &serde_json::Value) -> bool {
    fn nonempty(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::String(s) => !s.trim().is_empty(),
            serde_json::Value::Array(a) => a.iter().any(nonempty),
            _ => false,
        }
    }
    match v {
        serde_json::Value::Object(m) => {
            if m.get("type").and_then(serde_json::Value::as_str) == Some("hook_additional_context")
                && m.get("content").is_some_and(nonempty)
            {
                return true;
            }
            m.iter().any(|(k, val)| {
                ((k == "additionalContext"
                    || k == "additional_context"
                    || k == "hookAdditionalContext")
                    && nonempty(val))
                    || has_nonempty_additional_context(val)
            })
        }
        serde_json::Value::Array(a) => a.iter().any(has_nonempty_additional_context),
        _ => false,
    }
}

/// Most recently modified `*.jsonl` under `root`, recursively. Claude nests
/// by project and Codex nests by date, so a fixed depth is not portable.
pub fn newest_transcript(root: &Path) -> Option<std::path::PathBuf> {
    let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file()
                && path.extension().and_then(|e| e.to_str()) == Some("jsonl")
                && let Ok(mtime) = entry.metadata().and_then(|m| m.modified())
                && best.as_ref().is_none_or(|(t, _)| mtime > *t)
            {
                best = Some((mtime, path));
            }
        }
    }
    best.map(|(_, p)| p)
}

pub fn newest_transcript_among(roots: &[std::path::PathBuf]) -> Option<std::path::PathBuf> {
    roots
        .iter()
        .filter_map(|root| newest_transcript(root))
        .max_by_key(|path| std::fs::metadata(path).and_then(|m| m.modified()).ok())
}

/// Extracts (message id, usage) from one transcript line. Assistant lines
/// nest both under `message`; a top-level `id`/`usage` pair is accepted as a
/// fallback so a flattening schema change degrades gracefully.
fn usage_record(v: &serde_json::Value) -> Option<(String, [u64; 4])> {
    let msg = v.get("message");
    let id = msg
        .and_then(|m| m.get("id"))
        .or_else(|| v.get("id"))
        .and_then(|x| x.as_str())?;
    let usage = msg.and_then(|m| m.get("usage")).or_else(|| v.get("usage"))?;
    let obj = usage.as_object()?;
    if !USAGE_KEYS.iter().any(|k| obj.contains_key(*k)) {
        return None;
    }
    let g = |k: &str| obj.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
    Some((
        id.to_string(),
        [
            g("input_tokens"),
            g("output_tokens"),
            g("cache_read_input_tokens"),
            g("cache_creation_input_tokens"),
        ],
    ))
}

/// Codex emits cumulative session totals on `event_msg/token_count` records.
/// The last total is the session measurement; distinct totals approximate API
/// calls without summing cumulative values repeatedly.
fn codex_usage_record(v: &serde_json::Value) -> Option<[u64; 4]> {
    if v.get("type").and_then(serde_json::Value::as_str) != Some("event_msg")
        || v.pointer("/payload/type").and_then(serde_json::Value::as_str)
            != Some("token_count")
    {
        return None;
    }
    let usage = v.pointer("/payload/info/total_token_usage")?.as_object()?;
    if !["input_tokens", "output_tokens", "cached_input_tokens", "cache_write_input_tokens"]
        .iter()
        .any(|key| usage.contains_key(*key))
    {
        return None;
    }
    let get = |key: &str| usage.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0);
    Some([
        get("input_tokens"),
        get("output_tokens"),
        get("cached_input_tokens"),
        get("cache_write_input_tokens"),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant_line(id: &str, inp: u64, out: u64, cr: u64, cc: u64) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"id":"{id}","usage":{{"input_tokens":{inp},"output_tokens":{out},"cache_read_input_tokens":{cr},"cache_creation_input_tokens":{cc}}}}},"uuid":"u-{id}"}}"#
        )
    }

    #[test]
    fn last_usage_per_id_wins_and_distinct_ids_are_api_calls() {
        let lines = [
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#.to_string(),
            // Streaming repeats msg_a; only the LAST record may be booked.
            assistant_line("msg_a", 10, 1, 0, 5),
            assistant_line("msg_a", 10, 7, 0, 5),
            assistant_line("msg_b", 3, 2, 100, 0),
        ];
        let u = scan_text(&lines.join("\n")).unwrap();
        assert_eq!(u.api_calls, 2, "distinct message ids = api calls");
        assert_eq!(u.input_tokens, 13);
        assert_eq!(u.output_tokens, 9, "last record per id wins, not the sum");
        assert_eq!(u.cache_read_input_tokens, 100);
        assert_eq!(u.cache_creation_input_tokens, 5);
    }

    #[test]
    fn probe_refuses_majority_unparseable() {
        let mut lines: Vec<String> = (0..12).map(|i| format!("garbage line {i}")).collect();
        for i in 0..8 {
            lines.push(assistant_line(&format!("m{i}"), 1, 1, 0, 0));
        }
        assert!(scan_text(&lines.join("\n")).is_none(), "8/20 JSON yield is below 50%");
    }

    #[test]
    fn probe_accepts_exactly_half_and_skips_garbage_mid_file() {
        let mut lines: Vec<String> = (0..10).map(|i| format!("garbage line {i}")).collect();
        for i in 0..10 {
            lines.push(assistant_line(&format!("m{i}"), 1, 0, 0, 0));
        }
        // Usage past the probe window still counts (25th line).
        for i in 10..15 {
            lines.push(assistant_line(&format!("m{i}"), 1, 0, 0, 0));
        }
        let u = scan_text(&lines.join("\n")).unwrap();
        assert_eq!(u.api_calls, 15);
        assert_eq!(u.input_tokens, 15);
    }

    #[test]
    fn parseable_json_without_usage_records_is_none() {
        let lines: Vec<String> =
            (0..5).map(|i| format!(r#"{{"type":"progress","n":{i}}}"#)).collect();
        assert!(
            scan_text(&lines.join("\n")).is_none(),
            "JSON without usage = schema drift, never a measured zero"
        );
        // A "usage" object with only unknown keys is a different schema too.
        let alien = r#"{"id":"m1","usage":{"total_wibbles":9}}"#;
        assert!(scan_text(alien).is_none());
    }

    #[test]
    fn top_level_id_and_usage_fallback_is_accepted() {
        let line = r#"{"id":"m1","usage":{"input_tokens":4,"output_tokens":2}}"#;
        let u = scan_text(line).unwrap();
        assert_eq!((u.api_calls, u.input_tokens, u.output_tokens), (1, 4, 2));
        assert_eq!(u.cache_read_input_tokens, 0, "absent counters read as zero");
    }

    #[test]
    fn codex_cumulative_token_records_use_the_last_total() {
        let line = |input, cached, output| {
            format!(
                r#"{{"type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"cache_write_input_tokens":3,"output_tokens":{output},"reasoning_output_tokens":2,"total_tokens":999}}}}}}}}"#
            )
        };
        let text = [line(10, 5, 2), line(10, 5, 2), line(30, 12, 8)].join("\n");
        let usage = scan_text(&text).unwrap();
        assert_eq!(usage.api_calls, 2, "repeated cumulative records are deduplicated");
        assert_eq!(usage.input_tokens, 30);
        assert_eq!(usage.output_tokens, 8);
        assert_eq!(usage.cache_read_input_tokens, 12);
        assert_eq!(usage.cache_creation_input_tokens, 3);
    }

    #[test]
    fn missing_or_empty_file_is_none() {
        assert!(scan(Path::new("/nonexistent/cfetch-transcript.jsonl")).is_none());
        assert!(scan_text("").is_none());
        assert!(scan_text("\n  \n").is_none());
    }

    /// A hook_success-style record as the harness writes it: the invoked
    /// command plus (optionally) the hook's JSON output with its context.
    fn hook_line(cmd: &str, ctx: Option<&str>) -> String {
        match ctx {
            Some(c) => format!(
                r#"{{"type":"system","subtype":"hook_success","hookCommand":"{cmd}","output":{{"hookSpecificOutput":{{"hookEventName":"SessionStart","additionalContext":"{c}"}}}}}}"#
            ),
            None => {
                format!(r#"{{"type":"system","subtype":"hook_success","hookCommand":"{cmd}"}}"#)
            }
        }
    }

    #[test]
    fn verified_delivery_counts_firings_and_nonempty_context() {
        let lines = [
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#.to_string(),
            hook_line("'/usr/local/bin/cfetch' hook session-start", Some("[cfetch resident memory]")),
            hook_line("'/usr/local/bin/cfetch' hook stop", None),
            // Fired but delivered nothing: empty context must not count.
            hook_line("'/usr/local/bin/cfetch' hook pre-tool", Some("")),
            // Another tool's hook is not ours.
            hook_line("/opt/other-brain hook whatever", Some("noise")),
            assistant_line("msg_a", 1, 1, 0, 0),
        ];
        assert_eq!(verified_injections_text(&lines.join("\n")), Some((3, 1)));
    }

    #[test]
    fn verified_delivery_accepts_snake_case_and_drifted_nesting() {
        let line = r#"{"role":"system","content":[{"hook":{"command":"cfetch hook session-start","additional_context":"resident digest"}}]}"#;
        assert_eq!(verified_injections_text(line), Some((1, 1)));
    }

    #[test]
    fn prose_mentions_and_user_messages_are_not_hook_firings() {
        let user = r#"{"type":"user","message":{"role":"user","content":"please run cfetch hook stop"}}"#;
        let structured_but_user = r#"{"type":"user","message":{"role":"user","hookCommand":"'/usr/bin/cfetch' hook stop"}}"#;
        assert_eq!(verified_injections_text(user), None);
        assert_eq!(verified_injections_text(structured_but_user), None);
    }

    /// The shapes below are transcribed from real Claude Code 2.1.233
    /// transcripts. The previous tests used an invented `hookCommand` envelope,
    /// so they passed while the matcher recognized nothing the harness actually
    /// writes — the tests validated the bug.
    #[test]
    fn real_harness_shapes_are_recognized() {
        let success = r#"{"type":"attachment","attachment":{"type":"hook_success","hookName":"PostToolUse:Bash","hookEvent":"PostToolUse","content":"","stdout":"","stderr":"","exitCode":0,"command":"'/usr/bin/cfetch' hook post-tool","durationMs":5}}"#;
        assert_eq!(verified_injections_text(success), Some((1, 0)));

        let delivered = r#"{"type":"attachment","attachment":{"type":"hook_additional_context","content":["[cfetch: 11 staged candidate(s) await distillation]"],"hookName":"UserPromptSubmit","hookEvent":"UserPromptSubmit"}}"#;
        let fired = r#"{"type":"attachment","attachment":{"type":"hook_success","hookName":"UserPromptSubmit","hookEvent":"UserPromptSubmit","command":"'/usr/bin/cfetch' hook user-prompt","durationMs":7}}"#;
        assert_eq!(verified_injections_text(&format!("{fired}\n{delivered}")), Some((2, 1)));

        let stop_summary = r#"{"type":"system","subtype":"stop_hook_summary","hookCount":2,"hookInfos":[{"command":"bash ~/.claude/hooks/notify.sh","durationMs":17},{"command":"'/usr/bin/cfetch' hook stop","durationMs":21}],"hookErrors":[],"hookAdditionalContext":[]}"#;
        assert_eq!(verified_injections_text(stop_summary), Some((1, 0)));
    }

    /// A hook the harness cancelled FIRED and delivered nothing. Counting it as
    /// a non-firing would hide exactly the breakage this measurement exists for:
    /// the gap between fired and delivered is the health signal.
    #[test]
    fn a_timed_out_hook_counts_as_fired_but_not_delivered() {
        let cancelled = r#"{"type":"attachment","attachment":{"type":"hook_cancelled","hookName":"Stop","hookEvent":"Stop","command":"'/usr/bin/cfetch' hook stop","durationMs":10020,"timedOut":true,"timeoutMs":10000}}"#;
        assert_eq!(verified_injections_text(cancelled), Some((1, 0)));
    }

    /// A foreign hook in the same summary must not be attributed to us.
    #[test]
    fn another_tools_hook_in_the_stop_summary_is_not_ours() {
        let only_foreign = r#"{"type":"system","subtype":"stop_hook_summary","hookInfos":[{"command":"bash ~/.claude/hooks/notify.sh","durationMs":17}],"hookAdditionalContext":["something"]}"#;
        assert_eq!(verified_injections_text(only_foreign), None);
    }

    #[test]
    fn verified_delivery_refuses_garbage_and_unrecognized_formats() {
        // Majority non-JSON: the schema probe refuses the whole file.
        let mut lines: Vec<String> = (0..12).map(|i| format!("garbage {i}")).collect();
        for _ in 0..8 {
            lines.push(hook_line("cfetch hook stop", None));
        }
        assert_eq!(verified_injections_text(&lines.join("\n")), None);
        // Valid JSON but zero recognizable hook records: unverifiable — a
        // drifted format must never be reported as zero firings.
        let clean = [
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"progress","n":1}"#,
        ]
        .join("\n");
        assert_eq!(verified_injections_text(&clean), None);
        // A non-JSON line mentioning the command is not a record.
        let torn =
            [r#"{"type":"user","message":{"content":"x"}}"#, "ran cfetch hook stop by hand"]
                .join("\n");
        assert_eq!(verified_injections_text(&torn), None);
        assert_eq!(verified_injections_text(""), None);
        assert!(verified_injections(Path::new("/nonexistent/cfetch-t.jsonl")).is_none());
    }

    #[test]
    fn newest_transcript_picks_latest_jsonl_and_ignores_others() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("-home-x-repo");
        let proj = proj.join("2026/08/22");
        std::fs::create_dir_all(&proj).unwrap();
        let old = proj.join("old.jsonl");
        let new = proj.join("new.jsonl");
        std::fs::write(&old, "x").unwrap();
        std::fs::write(&new, "y").unwrap();
        std::fs::write(proj.join("notes.txt"), "z").unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(&old).unwrap();
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
            .unwrap();
        drop(f);
        assert_eq!(newest_transcript(dir.path()), Some(new));

        let empty = tempfile::tempdir().unwrap();
        assert_eq!(newest_transcript(empty.path()), None);
        assert_eq!(newest_transcript(Path::new("/nonexistent/cfetch-projects")), None);
    }
}
