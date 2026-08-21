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

use std::collections::BTreeMap;
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
    for line in &lines {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if let Some((id, usage)) = usage_record(&v) {
            per_id.insert(id, usage);
        }
    }
    // JSON parsed but zero usage records recognized: the schema drifted under
    // us. Refuse to report a measured zero.
    if per_id.is_empty() {
        return None;
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
    /// Our hook invocations as they appear in the harness's record of the
    /// command it ran (`'<abs path>/cfetch' hook <event>`).
    const HOOK_NEEDLE: &str = "cfetch hook";

    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() || !probe_ok(&lines) {
        return None;
    }
    let mut fired = 0u64;
    let mut delivered = 0u64;
    for line in &lines {
        if !line.contains(HOOK_NEEDLE) {
            continue;
        }
        // Only JSON-object records count — a stray mention inside prose (a
        // user message quoting the command) is not a hook record... but the
        // harness nests hook records inside larger objects, so the needle
        // check on the raw line is the recognizer and the parse is the gate.
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if !v.is_object() {
            continue;
        }
        fired += 1;
        if has_nonempty_additional_context(&v) {
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

/// Recursive search for a non-empty `additionalContext` (either harness
/// spelling) anywhere in the record — the exact nesting has drifted across
/// versions, the key name is the stable part.
fn has_nonempty_additional_context(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Object(m) => m.iter().any(|(k, val)| {
            ((k == "additionalContext" || k == "additional_context")
                && val.as_str().is_some_and(|s| !s.trim().is_empty()))
                || has_nonempty_additional_context(val)
        }),
        serde_json::Value::Array(a) => a.iter().any(has_nonempty_additional_context),
        _ => false,
    }
}

/// Most recently modified `*.jsonl` under `root`. The native layout is
/// `~/.claude/projects/<project-slug>/<session-id>.jsonl`, so one directory
/// level is walked; flat files directly under `root` are accepted too.
pub fn newest_transcript(root: &Path) -> Option<std::path::PathBuf> {
    let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    let mut consider = |p: std::path::PathBuf| {
        if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            return;
        }
        if let Ok(mtime) = std::fs::metadata(&p).and_then(|m| m.modified())
            && best.as_ref().is_none_or(|(t, _)| mtime > *t)
        {
            best = Some((mtime, p));
        }
    };
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&p) {
                for e in rd.flatten() {
                    consider(e.path());
                }
            }
        } else {
            consider(p);
        }
    }
    best.map(|(_, p)| p)
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
                r#"{{"type":"system","subtype":"hook_success","hookCommand":"'{cmd}'","output":{{"hookSpecificOutput":{{"hookEventName":"SessionStart","additionalContext":"{c}"}}}}}}"#
            ),
            None => {
                format!(r#"{{"type":"system","subtype":"hook_success","hookCommand":"'{cmd}'"}}"#)
            }
        }
    }

    #[test]
    fn verified_delivery_counts_firings_and_nonempty_context() {
        let lines = [
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#.to_string(),
            hook_line("/usr/local/bin/cfetch hook session-start", Some("[cfetch resident memory]")),
            hook_line("/usr/local/bin/cfetch hook stop", None),
            // Fired but delivered nothing: empty context must not count.
            hook_line("/usr/local/bin/cfetch hook pre-tool", Some("")),
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
