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
//!
//! The same per-call records also answer where the prompt cache went: a call
//! that re-creates a prefix instead of reading it pays for the whole
//! conversation again, so [`cache_rebuilds`] finds those calls and names what
//! changed at each boundary (see [`RebuildCause`]).

use std::path::Path;
use std::time::UNIX_EPOCH;

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

/// Reads and sums measured usage. `None` means "could not measure" — file
/// unreadable, schema probe refused, or no recognizable usage records.
pub fn scan(path: &Path) -> Option<TranscriptUsage> {
    let text = std::fs::read_to_string(path).ok()?;
    let agent = agent_session::agent_source_for_path(path)?;
    let updated = std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or(UNIX_EPOCH);
    scan_agent_text(agent, path, updated, &text)
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

fn scan_agent_text(
    agent: &str,
    path: &Path,
    updated: std::time::SystemTime,
    text: &str,
) -> Option<TranscriptUsage> {
    if agent != agent_session::AGENT_GEMINI {
        let lines: Vec<&str> = text.lines().filter(|line| !line.trim().is_empty()).collect();
        if lines.is_empty() || !probe_ok(&lines) {
            return None;
        }
    }
    let Some(session) = agent_session::parse_session_content(agent, path, updated, text) else {
        return (agent == agent_session::AGENT_CLAUDE).then(|| flat_usage(text)).flatten();
    };
    let positive_responses: Vec<_> = session
        .events
        .llm_responses
        .iter()
        .filter(|response| {
            response.input_tokens > 0
                || response.output_tokens > 0
                || response.cache_tokens > 0
                || response.total_tokens > 0
        })
        .collect();

    // agent-session's response IR deduplicates streamed Claude fragments by
    // source id and keeps the maximum counters. Its aggregate currently keeps
    // the first fragment, so use the IR for input/output and the split
    // aggregate for cache-read/cache-created until the IR exposes that split.
    let (mut input_tokens, mut output_tokens) = if agent == agent_session::AGENT_CLAUDE {
        (
            positive_responses.iter().map(|response| response.input_tokens).sum(),
            positive_responses.iter().map(|response| response.output_tokens).sum(),
        )
    } else {
        (nonnegative(session.usage.input_tokens), nonnegative(session.usage.output_tokens))
    };
    let mut cache_read_input_tokens = nonnegative(session.usage.cache_read_tokens);
    let mut cache_creation_input_tokens = nonnegative(session.usage.cache_creation_tokens);
    let mut codex_calls = 0;
    if agent == agent_session::AGENT_CODEX
        && let Some((calls, [input, output, cache_read, cache_created])) = codex_counters(text)
    {
        // Preserve the native counters exactly as recorded. agent-session
        // normalizes cached input out of `input_tokens`, which is useful for
        // cross-agent analytics but would change cfetch's existing ledger
        // field semantics and its cumulative watermarks during an upgrade.
        codex_calls = calls;
        input_tokens = input;
        output_tokens = output;
        cache_read_input_tokens = cache_read;
        cache_creation_input_tokens = cache_created;
    }
    if input_tokens == 0
        && output_tokens == 0
        && cache_read_input_tokens == 0
        && cache_creation_input_tokens == 0
    {
        // A parseable session without recognized counters is a measurement
        // gap, never a measured-looking zero.
        return None;
    }

    let mut api_calls = positive_responses.len() as u64;
    if agent == agent_session::AGENT_CODEX {
        // Codex may record cumulative token_count events without an adjacent
        // model-response body. Distinct cumulative totals are the best native
        // call boundary available in that transcript shape.
        api_calls = api_calls.max(codex_calls);
    }
    Some(TranscriptUsage {
        api_calls: api_calls.max(1),
        input_tokens,
        output_tokens,
        cache_read_input_tokens,
        cache_creation_input_tokens,
    })
}

fn nonnegative(value: i64) -> u64 {
    value.max(0) as u64
}

fn codex_counters(text: &str) -> Option<(u64, [u64; 4])> {
    let mut totals = std::collections::BTreeSet::new();
    let mut last = None;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if value.get("type").and_then(serde_json::Value::as_str) == Some("event_msg")
            && value.pointer("/payload/type").and_then(serde_json::Value::as_str)
                == Some("token_count")
            && let Some(usage) = value.pointer("/payload/info/total_token_usage")
        {
            let get = |key: &str| usage.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0);
            let counters = [
                get("input_tokens"),
                get("output_tokens"),
                get("cached_input_tokens"),
                get("cache_write_input_tokens"),
            ];
            totals.insert(counters);
            last = Some(counters);
        }
    }
    last.map(|last| (totals.len() as u64, last))
}

/// Minimal tolerance for a flattened Claude usage record. Native transcript
/// parsing belongs to agent-session; this one-record fallback preserves the
/// prior fail-soft contract for harness versions that briefly flattened the
/// `message` envelope.
fn flat_usage(text: &str) -> Option<TranscriptUsage> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let usage = value.get("usage")?.as_object()?;
    let get = |key: &str| usage.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0);
    let result = TranscriptUsage {
        api_calls: 1,
        input_tokens: get("input_tokens"),
        output_tokens: get("output_tokens"),
        cache_read_input_tokens: get("cache_read_input_tokens"),
        cache_creation_input_tokens: get("cache_creation_input_tokens"),
    };
    (result.input_tokens > 0
        || result.output_tokens > 0
        || result.cache_read_input_tokens > 0
        || result.cache_creation_input_tokens > 0)
        .then_some(result)
}

/// Both halves of the prefix-rebuild test. The absolute floor keeps ordinary
/// top-ups out — every turn appends a tool result to the warm prefix and pays
/// a little cache creation for it. The share test keeps a large but
/// proportionate build out, so the opening turns of a big session are not
/// reported as invalidation. Either half alone flags noise.
pub const REBUILD_MIN_CACHE_CREATION: u64 = 50_000;
const REBUILD_MIN_SHARE_PERCENT: u64 = 30;

/// Idle gap past which the prefix is gone whatever else happened: both
/// ephemeral cache lifetimes (5 minutes and 1 hour) have expired.
const REBUILD_IDLE_SECS: u64 = 65 * 60;

/// What changed at a rebuild boundary. Declaration order is how directly the
/// signal explains a rebuilt prefix, and it is the precedence
/// [`CacheRebuild::cause`] applies when several coincide: a replaced prefix
/// explains itself; a different model or harness version changes the prefix
/// or the key it is cached under; expiry comes last because it is also true
/// of most of the causes above it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RebuildCause {
    /// The harness compacted the conversation and the prefix it summarized
    /// no longer exists.
    Compaction,
    ModelSwitch,
    VersionChange,
    /// The cache expired before the next call.
    Idle,
    /// Nothing observable explains this rebuild. A real answer, not a missing
    /// one: sorting it into the nearest bucket would turn a measurement into
    /// a story, and the unexplained rebuilds are exactly the ones worth
    /// chasing.
    Unattributed,
}

impl RebuildCause {
    /// Stable identifiers — they reach the audit's JSON output.
    pub fn label(self) -> &'static str {
        match self {
            RebuildCause::Compaction => "compaction",
            RebuildCause::ModelSwitch => "model-switch",
            RebuildCause::VersionChange => "version-change",
            RebuildCause::Idle => "idle-over-65-min",
            RebuildCause::Unattributed => "unattributed",
        }
    }

    /// Every cause, in precedence order — the audit reports its buckets in it.
    pub const ALL: [RebuildCause; 5] = [
        RebuildCause::Compaction,
        RebuildCause::ModelSwitch,
        RebuildCause::VersionChange,
        RebuildCause::Idle,
        RebuildCause::Unattributed,
    ];
}

/// One prompt-cache prefix rebuild: an API call that paid to re-create the
/// cached prefix instead of reading it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheRebuild {
    /// Position among the counted API calls.
    pub call_index: u64,
    pub cache_creation_tokens: u64,
    /// Context size of the call before it — the share test's denominator.
    pub prior_context_tokens: u64,
    /// Gap since the previous call; `None` when either stamp was unusable.
    pub idle_secs: Option<u64>,
    /// EVERY signal present at this boundary, in precedence order. Empty means
    /// unattributed; more than one means the single cause below is a choice,
    /// not a deduction, and the report says so.
    pub signals: Vec<RebuildCause>,
}

impl CacheRebuild {
    pub fn cause(&self) -> RebuildCause {
        self.signals.first().copied().unwrap_or(RebuildCause::Unattributed)
    }
}

/// Rebuilds found in one transcript, with the call count they were found
/// among — a rebuild count means nothing without its denominator.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheRebuilds {
    pub api_calls: u64,
    pub rebuilds: Vec<CacheRebuild>,
}

impl CacheRebuilds {
    /// Tokens re-created by rebuilds — the bill this attribution explains.
    pub fn tokens(&self) -> u64 {
        self.rebuilds.iter().map(|r| r.cache_creation_tokens).sum()
    }
}

/// Detects prompt-cache prefix rebuilds and attributes a cause to each.
/// `None` means "could not measure" — unreadable file, schema probe refused,
/// or no per-call cache counters recognized. `Some` with no rebuilds is the
/// measured answer that the prefix held.
pub fn cache_rebuilds(path: &Path) -> Option<CacheRebuilds> {
    cache_rebuilds_text(&std::fs::read_to_string(path).ok()?)
}

fn cache_rebuilds_text(text: &str) -> Option<CacheRebuilds> {
    let lines: Vec<&str> = text.lines().filter(|line| !line.trim().is_empty()).collect();
    if lines.is_empty() || !probe_ok(&lines) {
        return None;
    }
    let calls = calls_in_order(text);
    if calls.is_empty() {
        // Parseable, but nothing carried per-call cache counters: the record
        // shape drifted, or this agent does not report the cache split.
        // Unmeasurable — never a measured "the prefix held".
        return None;
    }
    let mut rebuilds = Vec::new();
    // The first call BUILDS the prefix; there is nothing yet to invalidate.
    for (index, call) in calls.iter().enumerate().skip(1) {
        let previous = &calls[index - 1];
        let prior_context = previous.context();
        if call.cache_creation < REBUILD_MIN_CACHE_CREATION
            || prior_context == 0
            || call.cache_creation.saturating_mul(100)
                < prior_context.saturating_mul(REBUILD_MIN_SHARE_PERCENT)
        {
            continue;
        }
        let idle = match (previous.at, call.at) {
            (Some(before), Some(now)) => Some(now.saturating_sub(before)),
            _ => None,
        };
        let mut signals = Vec::new();
        if call.after_compaction {
            signals.push(RebuildCause::Compaction);
        }
        if changed(&previous.model, &call.model) {
            signals.push(RebuildCause::ModelSwitch);
        }
        if changed(&previous.version, &call.version) {
            signals.push(RebuildCause::VersionChange);
        }
        if idle.is_some_and(|seconds| seconds > REBUILD_IDLE_SECS) {
            signals.push(RebuildCause::Idle);
        }
        rebuilds.push(CacheRebuild {
            call_index: index as u64,
            cache_creation_tokens: call.cache_creation,
            prior_context_tokens: prior_context,
            idle_secs: idle,
            signals,
        });
    }
    Some(CacheRebuilds { api_calls: calls.len() as u64, rebuilds })
}

/// A value that is missing on either side is not a change. Attributing a
/// rebuild to a "switch" from an unrecorded model would invent the one thing
/// this measurement exists to avoid.
fn changed(before: &str, after: &str) -> bool {
    !before.is_empty() && !after.is_empty() && before != after
}

/// Per-call context accounting, in transcript order — only what a rebuild
/// boundary is judged on.
struct Call {
    id: String,
    model: String,
    version: String,
    /// Epoch seconds; `None` when the record carried no usable stamp.
    at: Option<u64>,
    input: u64,
    cache_read: u64,
    cache_creation: u64,
    /// A compaction boundary stands between this call and the one before it.
    after_compaction: bool,
}

impl Call {
    /// Everything the model read on this call: the prefix, warm or rebuilt,
    /// plus the fresh bytes.
    fn context(&self) -> u64 {
        self.input.saturating_add(self.cache_read).saturating_add(self.cache_creation)
    }
}

fn calls_in_order(text: &str) -> Vec<Call> {
    let mut calls: Vec<Call> = Vec::new();
    let mut by_id: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut after_compaction = false;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if is_compaction_boundary(&value) {
            after_compaction = true;
        }
        let Some(call) = usage_call(&value, after_compaction) else { continue };
        after_compaction = false;
        match by_id.get(&call.id) {
            // Streaming writes one record per fragment and resume replays old
            // lines: one id is one API call, and its LAST record carries the
            // final counters. The compaction flag survives from whichever
            // fragment saw the boundary.
            Some(&at) => {
                let seen_boundary = calls[at].after_compaction;
                calls[at] = Call { after_compaction: seen_boundary || call.after_compaction, ..call };
            }
            None => {
                if !call.id.is_empty() {
                    by_id.insert(call.id.clone(), calls.len());
                }
                calls.push(call);
            }
        }
    }
    calls
}

/// Compaction rewrites the prefix, so the record that announces it is the
/// only evidence that the next rebuild was structural. The three spellings
/// are the boundary marker, its metadata, and the summary message itself.
fn is_compaction_boundary(value: &serde_json::Value) -> bool {
    value.get("subtype").and_then(serde_json::Value::as_str) == Some("compact_boundary")
        || value.get("compactMetadata").is_some()
        || value.get("isCompactSummary").and_then(serde_json::Value::as_bool) == Some(true)
}

fn usage_call(value: &serde_json::Value, after_compaction: bool) -> Option<Call> {
    // A subagent's records are interleaved into the parent transcript and
    // carry their OWN cache prefix. Judging them against the main
    // conversation's context would report every subagent's first call as a
    // rebuild of a prefix it never used.
    if value.get("isSidechain").and_then(serde_json::Value::as_bool) == Some(true) {
        return None;
    }
    let message = value.get("message")?;
    let usage = message.get("usage")?.as_object()?;
    let get = |key: &str| usage.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0);
    let call = Call {
        id: message
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        model: message
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        version: value
            .get("version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        at: value.get("timestamp").and_then(serde_json::Value::as_str).and_then(epoch_secs),
        input: get("input_tokens"),
        cache_read: get("cache_read_input_tokens"),
        cache_creation: get("cache_creation_input_tokens"),
        after_compaction,
    };
    // The harness writes a placeholder assistant record with an all-zero
    // usage block for its own errors and interruptions, under a synthetic
    // model name. Keeping it would make the NEXT real call look like a model
    // switch — an attribution invented out of an API error.
    (call.input > 0 || call.cache_read > 0 || call.cache_creation > 0).then_some(call)
}

/// Epoch seconds from the harness's ISO-8601 UTC stamp
/// (`2026-08-20T21:49:31.142Z`). Deliberately strict, and UTC only: a stamp
/// carrying an unknown offset could shift a gap across the 65-minute line in
/// either direction, and dropping the idle signal is honest where guessing
/// the zone is not.
fn epoch_secs(stamp: &str) -> Option<u64> {
    let stamp = stamp.strip_suffix('Z')?;
    let (date, time) = stamp.split_once('T')?;
    let mut fields = date.split('-');
    let year: i64 = fields.next()?.parse().ok()?;
    let month: i64 = fields.next()?.parse().ok()?;
    let day: i64 = fields.next()?.parse().ok()?;
    if fields.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut fields = time.split(':');
    let hour: i64 = fields.next()?.parse().ok()?;
    let minute: i64 = fields.next()?.parse().ok()?;
    // Fractional seconds are ignored: the gap this feeds is measured against
    // a 65-minute threshold.
    let second: i64 = fields.next()?.split('.').next()?.parse().ok()?;
    if fields.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second;
    u64::try_from(seconds).ok()
}

/// Days since 1970-01-01 from a proleptic Gregorian date (Howard Hinnant's
/// civil-calendar algorithm). Written out because the only alternative is a
/// date crate, and this file needs exactly one number from one shape.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_shifted = (month + 9) % 12;
    let day_of_year = (153 * month_shifted + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
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
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
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
        || v.pointer("/message/role")
            .and_then(serde_json::Value::as_str)
            == Some("user")
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

/// Most recently modified transcript under one agent's root. File names,
/// extensions, nesting, and candidate timestamp rules belong to agent-session.
pub fn newest_transcript(agent: &'static str, root: &Path) -> Option<std::path::PathBuf> {
    agent_session::discover_session_files_in_dir(agent, root)
        .into_iter()
        .max_by_key(|candidate| candidate.updated)
        .map(|candidate| candidate.path)
}

pub fn newest_transcript_among(
    roots: &[(&'static str, std::path::PathBuf)],
) -> Option<std::path::PathBuf> {
    roots
        .iter()
        .filter_map(|(agent, root)| newest_transcript(agent, root))
        .max_by_key(|path| std::fs::metadata(path).and_then(|m| m.modified()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_text(text: &str) -> Option<TranscriptUsage> {
        scan_agent_text(
            agent_session::AGENT_CLAUDE,
            Path::new("/tmp/.claude/projects/test/session.jsonl"),
            UNIX_EPOCH,
            text,
        )
    }

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
        assert!(
            scan_text(&lines.join("\n")).is_none(),
            "8/20 JSON yield is below 50%"
        );
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
        let lines: Vec<String> = (0..5)
            .map(|i| format!(r#"{{"type":"progress","n":{i}}}"#))
            .collect();
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
        let usage = scan_agent_text(
            agent_session::AGENT_CODEX,
            Path::new("/tmp/.codex/sessions/2026/08/22/session.jsonl"),
            UNIX_EPOCH,
            &text,
        )
        .unwrap();
        assert_eq!(
            usage.api_calls, 2,
            "repeated cumulative records are deduplicated"
        );
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
            hook_line(
                "'/usr/local/bin/cfetch' hook session-start",
                Some("[cfetch resident memory]"),
            ),
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
        let user =
            r#"{"type":"user","message":{"role":"user","content":"please run cfetch hook stop"}}"#;
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
        assert_eq!(
            verified_injections_text(&format!("{fired}\n{delivered}")),
            Some((2, 1))
        );

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
        let torn = [
            r#"{"type":"user","message":{"content":"x"}}"#,
            "ran cfetch hook stop by hand",
        ]
                .join("\n");
        assert_eq!(verified_injections_text(&torn), None);
        assert_eq!(verified_injections_text(""), None);
        assert!(verified_injections(Path::new("/nonexistent/cfetch-t.jsonl")).is_none());
    }

    /// One assistant record as the harness writes it, with everything a
    /// rebuild boundary is judged on.
    fn call_line(
        id: &str,
        model: &str,
        version: &str,
        stamp: &str,
        input: u64,
        cache_read: u64,
        cache_creation: u64,
    ) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{stamp}","version":"{version}","isSidechain":false,"message":{{"id":"{id}","model":"{model}","usage":{{"input_tokens":{input},"output_tokens":40,"cache_read_input_tokens":{cache_read},"cache_creation_input_tokens":{cache_creation}}}}}}}"#
        )
    }

    /// A warm call: it read the prefix and appended a little.
    fn warm(id: &str, stamp: &str, context: u64) -> String {
        call_line(id, "model-a", "1.0.0", stamp, 20, context - 20, 0)
    }

    #[test]
    fn a_rebuild_needs_both_the_floor_and_the_share() {
        // 60k re-created over a 500k prior context is 12%: a big top-up, not
        // a rebuilt prefix.
        let proportionate = [
            warm("m1", "2026-08-20T10:00:00.000Z", 500_000),
            call_line("m2", "model-a", "1.0.0", "2026-08-20T10:00:30.000Z", 20, 460_000, 60_000),
        ]
        .join("\n");
        let found = cache_rebuilds_text(&proportionate).unwrap();
        assert_eq!(found.api_calls, 2);
        assert!(found.rebuilds.is_empty(), "12% of the prior context is not a rebuild");

        // 40k is 80% of a small prior context but under the absolute floor:
        // re-creating a tiny prefix is not the cost this measurement is for.
        let small = [
            warm("m1", "2026-08-20T10:00:00.000Z", 50_000),
            call_line("m3", "model-a", "1.0.0", "2026-08-20T10:00:30.000Z", 20, 0, 40_000),
        ]
        .join("\n");
        assert!(cache_rebuilds_text(&small).unwrap().rebuilds.is_empty(), "under the 50k floor");

        // Both halves satisfied: 60k re-created over a 100k prior context.
        let rebuilt = [
            warm("m1", "2026-08-20T10:00:00.000Z", 100_000),
            call_line("m4", "model-a", "1.0.0", "2026-08-20T10:00:30.000Z", 20, 0, 60_000),
        ]
        .join("\n");
        let found = cache_rebuilds_text(&rebuilt).unwrap();
        assert_eq!(found.rebuilds.len(), 1);
        assert_eq!(found.rebuilds[0].call_index, 1);
        assert_eq!(found.rebuilds[0].cache_creation_tokens, 60_000);
        assert_eq!(found.rebuilds[0].prior_context_tokens, 100_000);
        assert_eq!(found.tokens(), 60_000);
    }

    #[test]
    fn the_sessions_first_prefix_build_is_not_a_rebuild() {
        // Nothing was cached yet, so nothing was invalidated — counting the
        // opening build would put a cost on every session that has no cause.
        let opening = call_line(
            "m1",
            "model-a",
            "1.0.0",
            "2026-08-20T10:00:00.000Z",
            20,
            0,
            400_000,
        );
        let found = cache_rebuilds_text(&opening).unwrap();
        assert_eq!(found.api_calls, 1);
        assert!(found.rebuilds.is_empty());
    }

    #[test]
    fn an_unexplained_rebuild_stays_unattributed() {
        // Same model, same harness version, 30 seconds apart, no compaction:
        // the prefix was rebuilt and nothing observable says why. That must
        // survive as its own answer.
        let text = [
            warm("m1", "2026-08-20T10:00:00.000Z", 300_000),
            call_line("m2", "model-a", "1.0.0", "2026-08-20T10:00:30.000Z", 20, 0, 300_000),
        ]
        .join("\n");
        let found = cache_rebuilds_text(&text).unwrap();
        assert_eq!(found.rebuilds.len(), 1);
        assert!(found.rebuilds[0].signals.is_empty(), "no signal is not a weak signal");
        assert_eq!(found.rebuilds[0].cause(), RebuildCause::Unattributed);
        assert_eq!(found.rebuilds[0].idle_secs, Some(30));
    }

    #[test]
    fn each_cause_is_recognized_on_its_own() {
        let prior = warm("m1", "2026-08-20T10:00:00.000Z", 300_000);
        let rebuild = |model: &str, version: &str, stamp: &str| {
            [prior.clone(), call_line("m2", model, version, stamp, 20, 0, 300_000)].join("\n")
        };
        let cause = |text: &str| cache_rebuilds_text(text).unwrap().rebuilds[0].cause();

        assert_eq!(
            cause(&rebuild("model-b", "1.0.0", "2026-08-20T10:00:30.000Z")),
            RebuildCause::ModelSwitch
        );
        assert_eq!(
            cause(&rebuild("model-a", "1.1.0", "2026-08-20T10:00:30.000Z")),
            RebuildCause::VersionChange
        );
        // 66 minutes later: both ephemeral cache windows have expired.
        assert_eq!(
            cause(&rebuild("model-a", "1.0.0", "2026-08-20T11:06:00.000Z")),
            RebuildCause::Idle
        );
        // 64 minutes is inside the hour cache, so the clock explains nothing.
        assert_eq!(
            cause(&rebuild("model-a", "1.0.0", "2026-08-20T11:04:00.000Z")),
            RebuildCause::Unattributed
        );

        let compacted = [
            prior.clone(),
            r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"auto"}}"#
                .to_string(),
            call_line("m2", "model-a", "1.0.0", "2026-08-20T10:00:30.000Z", 20, 0, 300_000),
        ]
        .join("\n");
        assert_eq!(cause(&compacted), RebuildCause::Compaction);
    }

    #[test]
    fn coincident_signals_are_all_kept_and_ranked() {
        // Resumed the next morning on another model after a compaction: three
        // signals are true at once. The report must be able to say the single
        // cause was a choice.
        let text = [
            warm("m1", "2026-08-20T10:00:00.000Z", 300_000),
            r#"{"type":"user","isCompactSummary":true,"message":{"role":"user","content":"summary"}}"#
                .to_string(),
            call_line("m2", "model-b", "1.0.0", "2026-08-21T09:00:00.000Z", 20, 0, 300_000),
        ]
        .join("\n");
        let rebuild = cache_rebuilds_text(&text).unwrap().rebuilds.remove(0);
        assert_eq!(
            rebuild.signals,
            vec![RebuildCause::Compaction, RebuildCause::ModelSwitch, RebuildCause::Idle]
        );
        assert_eq!(rebuild.cause(), RebuildCause::Compaction, "the replaced prefix outranks the clock");
        assert_eq!(rebuild.idle_secs, Some(82_800));
    }

    #[test]
    fn an_error_placeholder_does_not_fake_a_model_switch() {
        // The harness records its own API errors as an assistant message with
        // an all-zero usage block under a synthetic model name. Booking it as
        // a call would make the next real call a "model switch" — a cause
        // invented out of an error.
        let text = [
            warm("m1", "2026-08-20T10:00:00.000Z", 300_000),
            call_line("m2", "<synthetic>", "1.0.0", "2026-08-20T10:00:10.000Z", 0, 0, 0),
            call_line("m3", "model-a", "1.0.0", "2026-08-20T10:00:30.000Z", 20, 0, 300_000),
        ]
        .join("\n");
        let found = cache_rebuilds_text(&text).unwrap();
        assert_eq!(found.api_calls, 2, "a zero-usage placeholder is not an api call");
        assert_eq!(found.rebuilds.len(), 1);
        assert_eq!(found.rebuilds[0].cause(), RebuildCause::Unattributed);
    }

    #[test]
    fn a_subagents_records_are_not_the_main_prefix() {
        // A sidechain runs its own conversation with its own cached prefix,
        // interleaved into this file. Its opening build is not an
        // invalidation here, and it must not become the denominator for the
        // next main-conversation call either.
        let sidechain = r#"{"type":"assistant","timestamp":"2026-08-20T10:00:10.000Z","version":"1.0.0","isSidechain":true,"message":{"id":"sub1","model":"model-a","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":900000}}}"#;
        let text = [
            warm("m1", "2026-08-20T10:00:00.000Z", 300_000),
            sidechain.to_string(),
            warm("m2", "2026-08-20T10:00:30.000Z", 320_000),
        ]
        .join("\n");
        let found = cache_rebuilds_text(&text).unwrap();
        assert_eq!(found.api_calls, 2, "sidechain calls belong to another conversation");
        assert!(found.rebuilds.is_empty());
    }

    #[test]
    fn streamed_fragments_of_one_call_are_one_call() {
        // Streaming repeats the id with growing counters. Treating fragments
        // as separate calls would compare a call against its own earlier
        // fragment and manufacture a rebuild out of a single request.
        let text = [
            warm("m1", "2026-08-20T10:00:00.000Z", 300_000),
            call_line("m2", "model-a", "1.0.0", "2026-08-20T10:00:30.000Z", 20, 0, 120_000),
            call_line("m2", "model-a", "1.0.0", "2026-08-20T10:00:31.000Z", 20, 0, 300_000),
        ]
        .join("\n");
        let found = cache_rebuilds_text(&text).unwrap();
        assert_eq!(found.api_calls, 2);
        assert_eq!(found.rebuilds.len(), 1);
        assert_eq!(
            found.rebuilds[0].cache_creation_tokens, 300_000,
            "the last fragment carries the final counters"
        );
    }

    #[test]
    fn a_compaction_boundary_survives_the_fragments_that_follow_it() {
        // The boundary is announced once; the call after it may be written as
        // several fragments, and only the first of them sees the flag.
        let text = [
            warm("m1", "2026-08-20T10:00:00.000Z", 300_000),
            r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"manual"}}"#.to_string(),
            call_line("m2", "model-a", "1.0.0", "2026-08-20T10:00:30.000Z", 20, 0, 100_000),
            call_line("m2", "model-a", "1.0.0", "2026-08-20T10:00:31.000Z", 20, 0, 300_000),
        ]
        .join("\n");
        let found = cache_rebuilds_text(&text).unwrap();
        assert_eq!(found.rebuilds.len(), 1);
        assert_eq!(found.rebuilds[0].cause(), RebuildCause::Compaction);
    }

    #[test]
    fn rebuild_attribution_refuses_unmeasurable_transcripts() {
        // Majority non-JSON: the shared schema probe refuses the file.
        let mut lines: Vec<String> = (0..12).map(|i| format!("garbage {i}")).collect();
        for i in 0..8 {
            lines.push(warm(&format!("m{i}"), "2026-08-20T10:00:00.000Z", 300_000));
        }
        assert!(cache_rebuilds_text(&lines.join("\n")).is_none());
        // Parseable JSON with no per-call cache counters: unmeasurable, never
        // a measured "the prefix held".
        let no_usage = [r#"{"type":"progress","n":1}"#, r#"{"type":"user","message":{}}"#].join("\n");
        assert!(cache_rebuilds_text(&no_usage).is_none());
        assert!(cache_rebuilds_text("").is_none());
        assert!(cache_rebuilds(Path::new("/nonexistent/cfetch-transcript.jsonl")).is_none());
    }

    #[test]
    fn an_unusable_timestamp_drops_the_idle_signal_only() {
        // A rebuild with no readable clock is still a rebuild; it simply
        // cannot be blamed on expiry.
        let text = [
            r#"{"type":"assistant","version":"1.0.0","message":{"id":"m1","model":"model-a","usage":{"input_tokens":20,"output_tokens":5,"cache_read_input_tokens":299980,"cache_creation_input_tokens":0}}}"#.to_string(),
            r#"{"type":"assistant","timestamp":"yesterday","version":"1.0.0","message":{"id":"m2","model":"model-a","usage":{"input_tokens":20,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":300000}}}"#.to_string(),
        ]
        .join("\n");
        let found = cache_rebuilds_text(&text).unwrap();
        assert_eq!(found.rebuilds.len(), 1);
        assert_eq!(found.rebuilds[0].idle_secs, None);
        assert_eq!(found.rebuilds[0].cause(), RebuildCause::Unattributed);
    }

    #[test]
    fn utc_stamps_parse_and_anything_else_is_refused() {
        assert_eq!(epoch_secs("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(epoch_secs("2026-08-20T21:49:31.142Z"), Some(1_787_262_571));
        // Leap day and year boundaries: the civil algorithm, not a 365-day
        // approximation.
        assert_eq!(epoch_secs("2024-02-29T00:00:00Z"), Some(1_709_164_800));
        assert_eq!(
            epoch_secs("2027-01-01T00:00:00Z").unwrap() - epoch_secs("2026-01-01T00:00:00Z").unwrap(),
            365 * 86_400
        );
        // An offset that is not UTC is refused rather than read as UTC: a
        // silent hour would move a gap across the 65-minute threshold.
        assert_eq!(epoch_secs("2026-08-20T21:49:31.142+02:00"), None);
        assert_eq!(epoch_secs("2026-08-20"), None);
        assert_eq!(epoch_secs("2026-13-01T00:00:00Z"), None);
        assert_eq!(epoch_secs("2026-08-20T25:00:00Z"), None);
        assert_eq!(epoch_secs("1969-12-31T23:59:59Z"), None, "pre-epoch has no unsigned answer");
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
        assert_eq!(
            newest_transcript(agent_session::AGENT_CLAUDE, dir.path()),
            Some(new)
        );

        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            newest_transcript(agent_session::AGENT_CLAUDE, empty.path()),
            None
        );
        assert_eq!(
            newest_transcript(
                agent_session::AGENT_CLAUDE,
                Path::new("/nonexistent/cfetch-projects")
            ),
            None
        );
    }
}
