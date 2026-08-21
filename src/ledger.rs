//! Injection booking: every character cfetch itself puts into a session is
//! recorded per source. A memory system that only counts what it saves and
//! never what it costs lies about its own value.
//!
//! The ledger is a fact of record, so it lives in the tree as an append-only
//! JSONL stream per host — `<brain_root>/logs/cfetch/ledger-<host>.jsonl` (see
//! [`crate::jsonl`]). The FILE is the ledger; [`Ledger`] is a derived
//! summary, folded on demand from every `ledger-*.jsonl` in the directory, so
//! `cfetch status` and `cfetch audit` show the whole fleet's bill without any
//! host having to ship its numbers anywhere.
//!
//! Booking keeps per-turn DELTA semantics for measured usage: Stop fires once
//! per turn and the transcript counters are cumulative for the session, so
//! each line records `max(0, current - booked)` and carries the cumulative
//! value it was computed against. That value is the watermark the next turn
//! reads back. Booking cumulative counters directly would inflate totals
//! 1+2+…+N-fold (upstream shipped exactly that bug: 898M fake tokens), and a
//! watermark that cannot be found is booked as ZERO rather than guessed —
//! re-arming from the new baseline instead of inventing usage.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::jsonl;

/// Stream name: files are `ledger-<host>.jsonl`.
pub const STREAM: &str = "ledger";

/// How far back a measured booking looks for its watermark. The previous
/// turn's line is normally within the last few hundred bytes; this bound only
/// matters for the first Stop of a session.
const WATERMARK_WINDOW_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SessionInjections {
    pub started_at: u64,
    /// source label -> (injections, chars, estimated tokens)
    #[serde(default)]
    pub by_source: BTreeMap<String, SourceTotals>,
    /// Sum of per-turn booked deltas — the honest measured total.
    #[serde(default)]
    pub measured: MeasuredUsage,
    /// Cumulative transcript counters as of the last booking (the watermark).
    #[serde(default)]
    pub booked: MeasuredUsage,
}

/// Token usage measured from the transcript — ground truth, kept strictly
/// apart from the estimated injection counters.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeasuredUsage {
    pub api_calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

impl MeasuredUsage {
    pub fn is_zero(&self) -> bool {
        *self == MeasuredUsage::default()
    }

    pub fn accumulate(&mut self, other: &MeasuredUsage) {
        self.api_calls += other.api_calls;
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
    }

    /// Per-metric `max(0, self - watermark)`: a counter that shrank (a reset
    /// or truncated transcript) books nothing rather than a negative.
    fn delta_over(&self, watermark: &MeasuredUsage) -> MeasuredUsage {
        MeasuredUsage {
            api_calls: self.api_calls.saturating_sub(watermark.api_calls),
            input_tokens: self.input_tokens.saturating_sub(watermark.input_tokens),
            output_tokens: self.output_tokens.saturating_sub(watermark.output_tokens),
            cache_read_input_tokens: self
                .cache_read_input_tokens
                .saturating_sub(watermark.cache_read_input_tokens),
            cache_creation_input_tokens: self
                .cache_creation_input_tokens
                .saturating_sub(watermark.cache_creation_input_tokens),
        }
    }
}

impl From<&crate::transcript::TranscriptUsage> for MeasuredUsage {
    fn from(u: &crate::transcript::TranscriptUsage) -> MeasuredUsage {
        MeasuredUsage {
            api_calls: u.api_calls,
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_input_tokens: u.cache_read_input_tokens,
            cache_creation_input_tokens: u.cache_creation_input_tokens,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SourceTotals {
    pub count: u64,
    pub chars: u64,
    pub tokens_estimated: u64,
}

/// The DERIVED view: every host's ledger stream folded into per-session
/// totals. Rebuildable from the tree at any moment; never a source of truth.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Ledger {
    /// session_id -> injections. BTreeMap for deterministic serialization.
    #[serde(default)]
    pub sessions: BTreeMap<String, SessionInjections>,
}

/// A folded ledger, the hosts whose lines went into it, and the stream files
/// that could not be read (a future format version) — reported, never silently
/// dropped.
#[derive(Debug, Default)]
pub struct Loaded {
    pub ledger: Ledger,
    /// Every host that has booked into this tree, in name order.
    pub hosts: std::collections::BTreeSet<String>,
    pub unreadable: Vec<String>,
}

/// Records an injection. Best-effort; never fails the calling hook.
pub fn book_injection(
    logs_dir: &Path,
    host: &str,
    max_bytes: u64,
    session_id: &str,
    source: &str,
    chars: usize,
) {
    if chars == 0 {
        return;
    }
    let _ = jsonl::append(
        logs_dir,
        STREAM,
        host,
        max_bytes,
        serde_json::json!({
            "kind": "inject",
            "session": session_id,
            "source": source,
            "chars": chars as u64,
            "tokens_estimated": crate::hook_io::estimate_tokens(chars),
        }),
    );
}

/// Books measured transcript usage for one turn. `current` is CUMULATIVE for
/// the session (what `transcript::scan` returns); only the delta above the
/// watermark is booked, and the line carries `current` as the next watermark.
/// Best-effort; never fails the calling hook.
pub fn book_measured(
    logs_dir: &Path,
    host: &str,
    max_bytes: u64,
    session_id: &str,
    current: &MeasuredUsage,
) {
    book_measured_within(logs_dir, host, max_bytes, session_id, current, WATERMARK_WINDOW_BYTES)
}

/// [`book_measured`] with an explicit watermark search budget (the bound the
/// "watermark lost" path depends on, so it is testable).
fn book_measured_within(
    logs_dir: &Path,
    host: &str,
    max_bytes: u64,
    session_id: &str,
    current: &MeasuredUsage,
    window: u64,
) {
    let (watermark, known) = match watermark_of(logs_dir, host, session_id, window) {
        Watermark::Found(w) => (w, true),
        // Never booked here before: the whole cumulative value is this
        // session's first delta.
        Watermark::Fresh => (MeasuredUsage::default(), true),
        // The watermark scrolled out of the window. Booking the cumulative
        // value now would double-count it; booking zero loses at most one
        // turn and re-arms the watermark exactly.
        Watermark::Unknown => (current.clone(), false),
    };
    let delta = current.delta_over(&watermark);
    let mut line = serde_json::json!({
        "kind": "measured",
        "session": session_id,
        "delta": delta,
        "cumulative": current,
    });
    if !known {
        line["watermark_lost"] = serde_json::Value::Bool(true);
    }
    let _ = jsonl::append(logs_dir, STREAM, host, max_bytes, line);
}

enum Watermark {
    /// The cumulative value booked for this session on this host.
    Found(MeasuredUsage),
    /// This host's whole ledger was scanned; this session was never booked.
    Fresh,
    /// The scan hit its budget without finding the session.
    Unknown,
}

/// Last cumulative counter booked for `session` on `host`, searched backwards
/// from the newest line.
fn watermark_of(logs_dir: &Path, host: &str, session: &str, window: u64) -> Watermark {
    let mut budget = window;
    let paths = jsonl::host_paths(logs_dir, STREAM, host);
    let mut complete = true;
    for path in &paths {
        if budget == 0 {
            complete = false;
            break;
        }
        let Some((text, truncated)) = jsonl::tail_text(path, budget) else { continue };
        budget = budget.saturating_sub(text.len() as u64);
        complete &= !truncated;
        for line in text.lines().rev() {
            let Some(rec) = jsonl::decode_line(line) else { continue };
            if rec.kind() != "measured" || rec.str("session") != session {
                continue;
            }
            let cumulative = rec
                .value("cumulative")
                .and_then(|v| serde_json::from_value::<MeasuredUsage>(v.clone()).ok());
            return match cumulative {
                Some(w) => Watermark::Found(w),
                // A measured line we cannot decode is worse than none: refuse
                // to guess and book nothing this turn.
                None => Watermark::Unknown,
            };
        }
    }
    if complete { Watermark::Fresh } else { Watermark::Unknown }
}

/// Folds EVERY host's ledger stream into the derived per-session view.
pub fn read(logs_dir: &Path) -> Loaded {
    let streams = jsonl::read_all(logs_dir, STREAM);
    let mut ledger = Ledger::default();
    let mut hosts = std::collections::BTreeSet::new();
    for rec in &streams.records {
        if !rec.host.is_empty() {
            hosts.insert(rec.host.clone());
        }
        let session = rec.str("session");
        if session.is_empty() {
            continue;
        }
        let ts = rec.ts.max(0) as u64;
        let entry = ledger
            .sessions
            .entry(session.to_string())
            .or_insert_with(|| SessionInjections { started_at: ts, ..Default::default() });
        entry.started_at = entry.started_at.min(ts);
        match rec.kind() {
            "inject" => {
                let source = rec.str("source");
                if source.is_empty() {
                    continue;
                }
                let totals = entry.by_source.entry(source.to_string()).or_default();
                totals.count += 1;
                totals.chars += rec.i64("chars").max(0) as u64;
                totals.tokens_estimated += rec.i64("tokens_estimated").max(0) as u64;
            }
            "measured" => {
                if let Some(delta) = rec
                    .value("delta")
                    .and_then(|v| serde_json::from_value::<MeasuredUsage>(v.clone()).ok())
                {
                    entry.measured.accumulate(&delta);
                }
                if let Some(cumulative) = rec
                    .value("cumulative")
                    .and_then(|v| serde_json::from_value::<MeasuredUsage>(v.clone()).ok())
                {
                    entry.booked = cumulative;
                }
            }
            _ => {}
        }
    }
    Loaded { ledger, hosts, unreadable: streams.unreadable }
}

pub fn load_from(logs_dir: &Path) -> Ledger {
    read(logs_dir).ledger
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: u64 = 1 << 20;

    fn usage(calls: u64, inp: u64, out: u64, cr: u64, cc: u64) -> MeasuredUsage {
        MeasuredUsage {
            api_calls: calls,
            input_tokens: inp,
            output_tokens: out,
            cache_read_input_tokens: cr,
            cache_creation_input_tokens: cc,
        }
    }

    #[test]
    fn booking_accumulates_per_source() {
        let dir = tempfile::tempdir().unwrap();
        book_injection(dir.path(), "h1", CAP, "s1", "resident", 700);
        book_injection(dir.path(), "h1", CAP, "s1", "resident", 300);
        book_injection(dir.path(), "h1", CAP, "s1", "banner", 35);
        let l = load_from(dir.path());
        let s = &l.sessions["s1"];
        assert_eq!(s.by_source["resident"].count, 2);
        assert_eq!(s.by_source["resident"].chars, 1000);
        assert!(s.by_source["resident"].tokens_estimated >= 285);
        assert_eq!(s.by_source["banner"].count, 1);
    }

    #[test]
    fn every_line_is_versioned_and_host_stamped() {
        let dir = tempfile::tempdir().unwrap();
        book_injection(dir.path(), "host-alpha", CAP, "s1", "resident", 10);
        let path = jsonl::stream_path(dir.path(), STREAM, "host-alpha");
        let line: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim()).unwrap();
        assert_eq!(line["v"], 1, "the shared format declares its version");
        assert_eq!(line["host"], "host-alpha");
        assert_eq!(line["kind"], "inject");
        assert_eq!(line["source"], "resident");
        assert_eq!(line["chars"], 10);
    }

    #[test]
    fn the_fleet_view_folds_every_hosts_stream() {
        // The point of moving the ledger into the tree: one status command
        // shows what every host booked, with no host shipping numbers anywhere.
        let dir = tempfile::tempdir().unwrap();
        book_injection(dir.path(), "host-alpha", CAP, "s-alpha", "resident", 100);
        book_injection(dir.path(), "host-beta", CAP, "s-beta", "resident", 250);
        book_measured(dir.path(), "host-beta", CAP, "s-beta", &usage(2, 40, 8, 0, 0));

        assert_eq!(jsonl::stream_paths(dir.path(), STREAM).len(), 2, "one file per host");
        let loaded = read(dir.path());
        assert_eq!(
            loaded.hosts.iter().cloned().collect::<Vec<_>>(),
            vec!["host-alpha".to_string(), "host-beta".to_string()],
            "the fold names every host that has booked here"
        );
        let l = loaded.ledger;
        assert_eq!(l.sessions.len(), 2);
        assert_eq!(l.sessions["s-alpha"].by_source["resident"].chars, 100);
        assert_eq!(l.sessions["s-beta"].by_source["resident"].chars, 250);
        assert_eq!(l.sessions["s-beta"].measured.input_tokens, 40);
    }

    #[test]
    fn measured_booking_records_per_turn_deltas_not_cumulative_sums() {
        let dir = tempfile::tempdir().unwrap();
        let t1 = usage(2, 100, 50, 10, 5);
        book_measured(dir.path(), "h1", CAP, "s1", &t1);
        let s1 = load_from(dir.path()).sessions["s1"].clone();
        assert_eq!(s1.measured, t1, "first booking takes the full cumulative value");
        assert_eq!(s1.booked, t1, "the line carries the next watermark");

        // Turn 2 reports CUMULATIVE counters again — booking must add only the
        // delta, so the total equals the cumulative value, not 1+2 sums.
        let t2 = usage(3, 160, 80, 40, 5);
        book_measured(dir.path(), "h1", CAP, "s1", &t2);
        let s2 = load_from(dir.path()).sessions["s1"].clone();
        assert_eq!(s2.measured, t2, "sum of booked deltas == cumulative counters");
        assert_eq!(s2.booked, t2);
    }

    #[test]
    fn measured_reset_to_zero_clamps_and_rearms_from_new_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let t1 = usage(2, 100, 50, 10, 5);
        book_measured(dir.path(), "h1", CAP, "s1", &t1);

        // Counters shrink (transcript reset/truncated): book nothing, but the
        // watermark must follow DOWN so later growth books again.
        book_measured(dir.path(), "h1", CAP, "s1", &MeasuredUsage::default());
        let s = load_from(dir.path()).sessions["s1"].clone();
        assert_eq!(s.measured, t1, "clamp at zero: a shrunken counter books no negative delta");
        assert!(s.booked.is_zero(), "watermark re-arms at the lower value");

        book_measured(dir.path(), "h1", CAP, "s1", &usage(1, 40, 8, 0, 0));
        let s = load_from(dir.path()).sessions["s1"].clone();
        assert_eq!(s.measured, usage(3, 140, 58, 10, 5), "post-reset growth is booked in full");
    }

    #[test]
    fn measured_watermarks_are_per_session_and_per_host() {
        let dir = tempfile::tempdir().unwrap();
        book_measured(dir.path(), "h1", CAP, "s1", &usage(1, 100, 0, 0, 0));
        book_measured(dir.path(), "h1", CAP, "s2", &usage(1, 70, 0, 0, 0));
        book_measured(dir.path(), "h1", CAP, "s1", &usage(2, 130, 0, 0, 0));
        let l = load_from(dir.path());
        assert_eq!(l.sessions["s1"].measured.input_tokens, 130, "s2 must not shift s1's watermark");
        assert_eq!(l.sessions["s2"].measured.input_tokens, 70);
    }

    #[test]
    fn measured_and_injection_booking_share_a_session_entry() {
        let dir = tempfile::tempdir().unwrap();
        book_injection(dir.path(), "h1", CAP, "s1", "resident", 100);
        book_measured(dir.path(), "h1", CAP, "s1", &usage(1, 10, 5, 0, 0));
        let l = load_from(dir.path());
        assert_eq!(l.sessions.len(), 1);
        let s = &l.sessions["s1"];
        assert_eq!(s.by_source["resident"].chars, 100);
        assert_eq!(s.measured.input_tokens, 10);
    }

    #[test]
    fn zero_chars_books_nothing() {
        let dir = tempfile::tempdir().unwrap();
        book_injection(dir.path(), "h1", CAP, "s1", "resident", 0);
        assert!(load_from(dir.path()).sessions.is_empty());
        assert!(
            jsonl::stream_paths(dir.path(), STREAM).is_empty(),
            "a no-op booking must not create a stream"
        );
    }

    #[test]
    fn retention_caps_the_stream_at_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..400 {
            book_injection(dir.path(), "h1", 900, &format!("s{i}"), "resident", 10);
        }
        let files = jsonl::stream_paths(dir.path(), STREAM);
        assert!(files.len() > 1, "the byte cap rotated the stream");
        assert!(files.len() <= jsonl::MAX_ROTATIONS + 1);
        let total: u64 = files.iter().map(|p| std::fs::metadata(p).unwrap().len()).sum();
        assert!(total <= 900 * 3 + 256, "the ledger footprint stays bounded: {total}");
        assert!(!load_from(dir.path()).sessions.is_empty(), "recent bookings survive");
    }

    #[test]
    fn a_torn_last_line_does_not_wedge_booking() {
        let dir = tempfile::tempdir().unwrap();
        book_injection(dir.path(), "h1", CAP, "s1", "resident", 100);
        let path = jsonl::stream_path(dir.path(), STREAM, "h1");
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{\"v\":1,\"ts\":9,\"host\":\"h1\",\"kind\":\"inj");
        std::fs::write(&path, raw).unwrap();

        book_injection(dir.path(), "h1", CAP, "s1", "resident", 50);
        let l = load_from(dir.path());
        assert_eq!(
            l.sessions["s1"].by_source["resident"].chars, 150,
            "the torn line is skipped and booking continues"
        );
    }

    #[test]
    fn an_unknown_format_version_is_reported_not_folded() {
        let dir = tempfile::tempdir().unwrap();
        book_injection(dir.path(), "h1", CAP, "s1", "resident", 100);
        std::fs::write(
            dir.path().join("ledger-h9.jsonl"),
            "{\"v\":99,\"ts\":1,\"host\":\"h9\",\"kind\":\"inject\",\"session\":\"s9\",\
             \"source\":\"resident\",\"chars\":1}\n",
        )
        .unwrap();
        let loaded = read(dir.path());
        assert_eq!(loaded.unreadable.len(), 1, "the refusal names the file");
        assert!(loaded.unreadable[0].contains("ledger-h9.jsonl"));
        assert!(!loaded.ledger.sessions.contains_key("s9"), "a format we cannot read is not guessed");
        assert!(loaded.ledger.sessions.contains_key("s1"), "readable hosts still fold");
    }

    #[test]
    fn a_lost_watermark_books_zero_instead_of_double_counting() {
        // The previous turn's line scrolled out of the search window. Booking
        // the cumulative value again would inflate the total 1+2+…+N-fold
        // (the 898M-token bug class); booking zero loses one turn and re-arms
        // the watermark exactly.
        let dir = tempfile::tempdir().unwrap();
        book_measured(dir.path(), "h1", CAP, "s1", &usage(5, 500, 100, 0, 0));
        for i in 0..40 {
            book_injection(dir.path(), "h1", CAP, &format!("noise{i}"), "resident", 10);
        }
        // A window too small to reach back past the noise.
        book_measured_within(dir.path(), "h1", CAP, "s1", &usage(6, 600, 120, 0, 0), 200);
        let s = load_from(dir.path()).sessions["s1"].clone();
        assert_eq!(
            s.measured,
            usage(5, 500, 100, 0, 0),
            "an unfindable watermark books ZERO, never the cumulative value again"
        );
        assert_eq!(s.booked, usage(6, 600, 120, 0, 0), "the watermark still re-arms");
        // The loss is stated in the line itself, not hidden.
        let raw = std::fs::read_to_string(jsonl::stream_path(dir.path(), STREAM, "h1")).unwrap();
        assert!(raw.contains("watermark_lost"));

        // With the full window the very next turn books its delta normally.
        book_measured(dir.path(), "h1", CAP, "s1", &usage(7, 700, 140, 0, 0));
        let s = load_from(dir.path()).sessions["s1"].clone();
        assert_eq!(s.measured, usage(6, 600, 120, 0, 0), "delta over the re-armed watermark");
    }

    #[test]
    fn a_fresh_session_books_its_full_cumulative_value() {
        // A complete scan that finds nothing is not a lost watermark: the
        // session has simply never been booked here.
        let dir = tempfile::tempdir().unwrap();
        book_injection(dir.path(), "h1", CAP, "other", "resident", 10);
        book_measured(dir.path(), "h1", CAP, "s1", &usage(4, 400, 80, 0, 0));
        let s = load_from(dir.path()).sessions["s1"].clone();
        assert_eq!(s.measured, usage(4, 400, 80, 0, 0));
    }
}
