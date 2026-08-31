//! Versioned append-only JSONL streams in the shared tree.
//!
//! Ring-6 exhaust and the injection ledger are data OF RECORD, so they live in
//! the tree beside the markdown instead of in a per-host database: a candidate
//! flagged on one host has to be visible to a distillation session on another,
//! and a database file on one machine can never be that.
//!
//! Line shape: one JSON object per line, carrying `v` (format version), `ts`
//! (unix seconds), `host`, and whatever the caller adds; keys are serialized
//! in sorted order, so two hosts writing the same event write the same bytes.
//! The version is EXPLICIT and checked on read — a line written by a format
//! this binary does not know makes its file unreadable (reported by name),
//! never guessed at. A torn last line (crash mid-append) is skipped and
//! counted; that is the only loss the format tolerates.
//!
//! Concurrency: the FILE NAME carries the host, so two hosts never write the
//! same file. Same-host processes append with `O_APPEND` in ONE `write` of one
//! short line, which the kernel does not interleave. No fsync — a hook that
//! waits for the disk is a hook that stalls the agent, and the cost of a lost
//! tail is a handful of ring-6 events.
//!
//! Retention is enforced at the writer, by bytes: when a line would push the
//! live file past `max_bytes` it rotates to `<stem>-<host>.1.jsonl`, older
//! generations shift down, and the generation beyond [`MAX_ROTATIONS`] is
//! dropped. Nothing of record is lost by rotation that a human still needs:
//! ring-5 candidates are separate files in the tree, not stream entries.

use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use anyhow::Context as _;

/// Line format version. Bump only for an incompatible change; readers refuse
/// anything else rather than guessing at unknown fields.
pub const FORMAT_VERSION: u64 = 1;

/// Rotated generations kept beside the live file (`.1` and `.2`). The stream's
/// total footprint per host is therefore at most 3 x `max_bytes`.
pub const MAX_ROTATIONS: usize = 2;

/// One decoded line. `fields` holds everything but the envelope keys.
#[derive(Debug, Clone)]
pub struct Record {
    pub ts: i64,
    pub host: String,
    pub fields: serde_json::Map<String, serde_json::Value>,
}

impl Record {
    pub fn str(&self, key: &str) -> &str {
        self.fields.get(key).and_then(serde_json::Value::as_str).unwrap_or_default()
    }

    pub fn i64(&self, key: &str) -> i64 {
        self.fields.get(key).and_then(serde_json::Value::as_i64).unwrap_or(0)
    }

    pub fn value(&self, key: &str) -> Option<&serde_json::Value> {
        self.fields.get(key)
    }

    pub fn kind(&self) -> &str {
        self.str("kind")
    }
}

/// The result of reading one or more stream files.
#[derive(Debug, Default)]
pub struct Streams {
    pub records: Vec<Record>,
    /// Files refused whole, `"<path>: <reason>"` — a future format version, or
    /// an unreadable file. Surfaced by callers; never silently dropped.
    pub unreadable: Vec<String>,
    /// Lines skipped as unparseable (a torn append).
    pub torn_lines: usize,
}

impl Streams {
    fn absorb(&mut self, other: Streams) {
        self.records.extend(other.records);
        self.unreadable.extend(other.unreadable);
        self.torn_lines += other.torn_lines;
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Filename-safe host token: anything outside `[A-Za-z0-9_-]` becomes `-`.
/// Dots are REPLACED, not preserved: the rotation suffix grammar is
/// `<stem>-<host>.<n>.jsonl`, so a dot inside the host token lets one host's
/// live stream be another host's rotated generation (`exhaust-10.0.0.jsonl`
/// vs `exhaust-10.0.0.1.jsonl`) - rotation would then delete or overwrite
/// the other host's data of record.
pub fn sanitize_host(host: &str) -> String {
    let cleaned: String = host
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') { c } else { '-' })
        .collect();
    if cleaned.is_empty() { "unknown-host".to_string() } else { cleaned }
}

/// Live stream file for one host: `<dir>/<stem>-<host>.jsonl`.
pub fn stream_path(dir: &Path, stem: &str, host: &str) -> PathBuf {
    dir.join(format!("{stem}-{}.jsonl", sanitize_host(host)))
}

/// Rotated generation `n` (1-based) of a live stream path.
fn rotated_path(live: &Path, n: usize) -> PathBuf {
    let name = live.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let base = name.strip_suffix(".jsonl").unwrap_or(&name);
    live.with_file_name(format!("{base}.{n}.jsonl"))
}

/// Live + rotated files for ONE host, newest first.
pub fn host_paths(dir: &Path, stem: &str, host: &str) -> Vec<PathBuf> {
    let live = stream_path(dir, stem, host);
    let mut paths = vec![live.clone()];
    paths.extend((1..=MAX_ROTATIONS).map(|n| rotated_path(&live, n)));
    paths
}

/// Every stream file under `dir` for `stem`, all hosts and all rotations, in
/// deterministic name order. This is what makes the fleet view work: the
/// reader never needs to know which hosts exist.
pub fn stream_paths(dir: &Path, stem: &str) -> Vec<PathBuf> {
    let prefix = format!("{stem}-");
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut paths: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with(&prefix) && n.ends_with(".jsonl") && p.is_file()
            })
        })
        .collect();
    paths.sort();
    paths
}

/// Appends one line to this host's stream, rotating first when the line would
/// cross `max_bytes`. `fields` must be a JSON object; its keys are merged OVER
/// the envelope (`v`, `ts`, `host`), so a caller carrying historical records
/// into the stream can supply their original `ts`.
pub fn append(
    dir: &Path,
    stem: &str,
    host: &str,
    max_bytes: u64,
    fields: serde_json::Value,
) -> anyhow::Result<()> {
    let serde_json::Value::Object(extra) = fields else {
        anyhow::bail!("jsonl append expects a JSON object");
    };
    let mut obj = serde_json::Map::new();
    obj.insert("v".into(), serde_json::Value::from(FORMAT_VERSION));
    obj.insert("ts".into(), serde_json::Value::from(now()));
    obj.insert("host".into(), serde_json::Value::from(sanitize_host(host)));
    for (k, v) in extra {
        obj.insert(k, v);
    }
    let mut line = serde_json::to_string(&serde_json::Value::Object(obj))?;
    line.push('\n');

    std::fs::create_dir_all(dir)
        .with_context(|| format!("create log dir {}", dir.display()))?;
    let path = stream_path(dir, stem, host);
    rotate_if_needed(&path, max_bytes, line.len() as u64);
    // O_APPEND: the offset is chosen by the kernel under the inode lock, so
    // concurrent same-host writers cannot overwrite each other's lines.
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        // Readable too: the terminator check below needs the last byte, and
        // reads on an O_APPEND handle cannot disturb where writes land.
        .read(true)
        .open(&path)
        .with_context(|| format!("append {}", path.display()))?;
    // A crash can leave a partial line with no terminator. Starting the next
    // record with its own newline keeps the damage to that one line instead of
    // gluing the torn tail onto a perfectly good new record.
    if ends_mid_line(&mut f) {
        line.insert(0, '\n');
    }
    f.write_all(line.as_bytes()).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Does the file end in the middle of a line? One seek and one byte.
fn ends_mid_line(f: &mut std::fs::File) -> bool {
    let Ok(len) = f.metadata().map(|m| m.len()) else { return false };
    if len == 0 {
        return false;
    }
    let mut last = [0u8; 1];
    if f.seek(SeekFrom::Start(len - 1)).is_err() || f.read_exact(&mut last).is_err() {
        return false;
    }
    last[0] != b'\n'
}

/// Rotates the live file when the pending line would push it past the cap.
/// Best effort: a failed rotation must not lose the event, so the append
/// proceeds either way (the cap is then enforced by the next writer).
fn rotate_if_needed(live: &Path, max_bytes: u64, pending: u64) {
    let Ok(meta) = std::fs::metadata(live) else { return };
    if meta.len() == 0 || meta.len() + pending <= max_bytes {
        return;
    }
    let _ = std::fs::remove_file(rotated_path(live, MAX_ROTATIONS));
    for n in (1..MAX_ROTATIONS).rev() {
        let _ = std::fs::rename(rotated_path(live, n), rotated_path(live, n + 1));
    }
    let _ = std::fs::rename(live, rotated_path(live, 1));
}

/// Decodes one line. `Ok(None)` = torn/unparseable line (skip it),
/// `Err` = a version this binary must not interpret.
fn decode(line: &str) -> Result<Option<Record>, u64> {
    let Ok(serde_json::Value::Object(mut obj)) = serde_json::from_str(line) else {
        return Ok(None);
    };
    let Some(v) = obj.get("v").and_then(serde_json::Value::as_u64) else {
        return Ok(None);
    };
    if v != FORMAT_VERSION {
        return Err(v);
    }
    let ts = obj.get("ts").and_then(serde_json::Value::as_i64).unwrap_or(0);
    let host = obj.get("host").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
    for key in ["v", "ts", "host"] {
        obj.remove(key);
    }
    Ok(Some(Record { ts, host, fields: obj }))
}

fn decode_all(path: &Path, text: &str) -> Streams {
    let mut out = Streams::default();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match decode(line) {
            Ok(Some(rec)) => out.records.push(rec),
            Ok(None) => out.torn_lines += 1,
            Err(v) => {
                // Refuse the whole file: a stream written in a format this
                // binary does not know is not partially trustworthy.
                out.records.clear();
                out.unreadable.push(format!(
                    "{}: line format v{v}, this binary reads v{FORMAT_VERSION}",
                    path.display()
                ));
                return out;
            }
        }
    }
    out
}

/// Reads every stream file for `stem` (all hosts, all rotations), oldest
/// record first. Records are ordered by timestamp with file order preserved
/// inside one second, so a single host's appends keep their sequence.
pub fn read_all(dir: &Path, stem: &str) -> Streams {
    let mut out = Streams::default();
    for path in stream_paths(dir, stem) {
        match std::fs::read_to_string(&path) {
            Ok(text) => out.absorb(decode_all(&path, &text)),
            Err(e) => out.unreadable.push(format!("{}: {e}", path.display())),
        }
    }
    out.records.sort_by_key(|r| r.ts);
    out
}

/// Last `max_bytes` of one file as whole lines, plus whether the read was
/// TRUNCATED (older lines exist that the caller did not see). The first,
/// possibly partial, line of a truncated read is dropped.
///
/// The truncation flag is what lets a caller tell "this file holds nothing
/// for me" apart from "I did not look far enough" — the difference between a
/// fresh session and a lost watermark.
pub fn tail_text(path: &Path, max_bytes: u64) -> Option<(String, bool)> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let mut buf = Vec::new();
    let truncated = len > max_bytes;
    if truncated {
        f.seek(SeekFrom::Start(len - max_bytes)).ok()?;
        f.read_to_end(&mut buf).ok()?;
        let cut = buf.iter().position(|b| *b == b'\n').map(|i| i + 1).unwrap_or(buf.len());
        buf.drain(..cut);
    } else {
        f.read_to_end(&mut buf).ok()?;
    }
    Some((String::from_utf8_lossy(&buf).into_owned(), truncated))
}

/// Decodes ONE line, version-checked. `None` for a torn line and for a
/// version this binary does not read — a caller scanning line by line refuses
/// both the same way: by ignoring what it cannot interpret.
pub fn decode_line(line: &str) -> Option<Record> {
    decode(line).ok().flatten()
}

/// Reads at most `max_bytes` from the END of ONE host's stream (live file
/// first, then rotated generations until the budget is spent), oldest record
/// first and in append order — no timestamp sort, because the traps depend on
/// "this failed BEFORE that succeeded".
///
/// A bounded window is what keeps the Stop hook's cost constant: the traps are
/// heuristics over recent behavior, not a full-history query.
pub fn read_tail(dir: &Path, stem: &str, host: &str, max_bytes: u64) -> Streams {
    let mut budget = max_bytes;
    let mut chunks: Vec<Streams> = Vec::new();
    for path in host_paths(dir, stem, host) {
        if budget == 0 {
            break;
        }
        let Some((text, _truncated)) = tail_text(&path, budget) else { continue };
        budget = budget.saturating_sub(text.len() as u64);
        chunks.push(decode_all(&path, &text));
    }
    // host_paths is newest-first; the caller wants oldest-first.
    chunks.reverse();
    let mut out = Streams::default();
    for c in chunks {
        out.absorb(c);
    }
    out
}

/// Byte count for a human: exact below a kibibyte, one decimal above it. A
/// footprint reported as "0 KiB" tells a reader nothing about whether capture
/// is running.
pub fn human_bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    let n = n as f64;
    if n < K {
        format!("{n} B")
    } else if n < K * K {
        format!("{:.1} KiB", n / K)
    } else {
        format!("{:.1} MiB", n / (K * K))
    }
}

/// Total bytes of every stream file for `stem` — the footprint a reporting
/// surface can show without reading a single line.
pub fn footprint(dir: &Path, stem: &str) -> u64 {
    stream_paths(dir, stem)
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read_lines(path: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn appended_lines_carry_the_version_envelope() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), "exhaust", "h1", 1 << 20, json!({"kind": "bash", "n": 1})).unwrap();
        let path = stream_path(dir.path(), "exhaust", "h1");
        let lines = read_lines(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["v"], 1, "every line declares its format version");
        assert_eq!(lines[0]["host"], "h1");
        assert_eq!(lines[0]["kind"], "bash");
        assert!(lines[0]["ts"].as_i64().unwrap() > 0);
    }

    #[test]
    fn each_host_writes_its_own_file_and_reads_see_all_of_them() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), "exhaust", "alpha", 1 << 20, json!({"kind": "a"})).unwrap();
        append(dir.path(), "exhaust", "beta", 1 << 20, json!({"kind": "b"})).unwrap();
        assert!(stream_path(dir.path(), "exhaust", "alpha").exists());
        assert!(stream_path(dir.path(), "exhaust", "beta").exists());
        assert_eq!(
            read_lines(&stream_path(dir.path(), "exhaust", "alpha")).len(),
            1,
            "concurrent hosts never share a file, so their lines cannot interleave"
        );
        let all = read_all(dir.path(), "exhaust");
        let hosts: Vec<&str> = all.records.iter().map(|r| r.host.as_str()).collect();
        assert_eq!(hosts.len(), 2);
        assert!(hosts.contains(&"alpha") && hosts.contains(&"beta"));
    }

    #[test]
    fn hosts_are_sanitized_into_the_file_name() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), "exhaust", "../evil host", 1 << 20, json!({"kind": "a"})).unwrap();
        let files: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            files,
            vec!["exhaust----evil-host.jsonl".to_string()],
            "a host id can never carry a path separator (or a dot - see the rotation-collision test) into the file name"
        );
    }

    #[test]
    fn dotted_host_ids_cannot_collide_with_rotation_generations() {
        // `exhaust-10.0.0.jsonl` is host `10.0.0`'s live stream AND host
        // `10.0.0.1`'s live stream when dots survive sanitizing - rotation of
        // one would delete the other's data of record. Dots are replaced so
        // the two hosts can never share a file name.
        let dir = tempfile::tempdir().unwrap();
        let a = stream_path(dir.path(), "exhaust", "10.0.0");
        let b = stream_path(dir.path(), "exhaust", "10.0.0.1");
        assert_ne!(a, b, "distinct hosts must map to distinct files");
        let a2 = stream_path(dir.path(), "exhaust", "10.0.0.1");
        assert_eq!(a.file_name().unwrap().to_string_lossy().chars().filter(|c| *c == '.').count(), 1, "exactly one dot: the .jsonl suffix");
        assert!(!a.to_string_lossy().contains("10.0.0."), "dot inside the host token must not survive: {a:?}");
        assert_eq!(a2, b);
    }

    #[test]
    fn rotation_caps_the_stream_at_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let live = stream_path(dir.path(), "exhaust", "h1");
        // ~90 bytes per line; a 400 byte cap rotates every few appends.
        for i in 0..60 {
            append(dir.path(), "exhaust", "h1", 400, json!({"kind": "bash", "i": i})).unwrap();
        }
        assert!(live.exists());
        assert!(rotated_path(&live, 1).exists(), "the previous generation is kept");
        assert!(!rotated_path(&live, MAX_ROTATIONS + 1).exists(), "at most 2 rotations");
        let total: u64 = stream_paths(dir.path(), "exhaust")
            .iter()
            .map(|p| std::fs::metadata(p).unwrap().len())
            .sum();
        assert!(total <= 400 * (MAX_ROTATIONS as u64 + 1), "footprint stays bounded: {total}");
        // Rotated generations are still record: reads span them.
        let all = read_all(dir.path(), "exhaust");
        assert!(all.records.len() > 4, "rotated lines stay readable: {}", all.records.len());
        assert!(all.unreadable.is_empty());
    }

    #[test]
    fn a_future_version_makes_its_file_unreadable_instead_of_guessed_at() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), "exhaust", "h1", 1 << 20, json!({"kind": "ok"})).unwrap();
        let future = dir.path().join("exhaust-h9.jsonl");
        std::fs::write(&future, "{\"v\":2,\"ts\":1,\"host\":\"h9\",\"kind\":\"new\"}\n").unwrap();
        let all = read_all(dir.path(), "exhaust");
        assert_eq!(all.unreadable.len(), 1, "the refusal is reported by name");
        assert!(all.unreadable[0].contains("exhaust-h9.jsonl"));
        assert!(all.unreadable[0].contains("v2"));
        assert_eq!(all.records.len(), 1, "the readable host's stream still answers");
        assert_eq!(all.records[0].kind(), "ok");
    }

    #[test]
    fn an_unterminated_tail_never_swallows_the_next_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = stream_path(dir.path(), "exhaust", "h1");
        append(dir.path(), "exhaust", "h1", 1 << 20, json!({"kind": "first"})).unwrap();
        // A crash mid-append: a partial line with no terminator.
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{\"v\":1,\"ts\":9,\"host\":\"h1\",\"kin");
        std::fs::write(&path, raw).unwrap();

        append(dir.path(), "exhaust", "h1", 1 << 20, json!({"kind": "second"})).unwrap();
        let all = read_all(dir.path(), "exhaust");
        let kinds: Vec<&str> = all.records.iter().map(|r| r.kind()).collect();
        assert_eq!(kinds, vec!["first", "second"], "only the torn line is lost");
        assert_eq!(all.torn_lines, 1);
    }

    #[test]
    fn a_torn_line_is_skipped_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        let path = stream_path(dir.path(), "exhaust", "h1");
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(
            &path,
            "{\"v\":1,\"ts\":1,\"host\":\"h1\",\"kind\":\"a\"}\n{\"v\":1,\"ts\":2,\"ho",
        )
        .unwrap();
        let all = read_all(dir.path(), "exhaust");
        assert_eq!(all.records.len(), 1);
        assert_eq!(all.torn_lines, 1);
        assert!(all.unreadable.is_empty(), "a torn tail is not a version refusal");
    }

    #[test]
    fn tail_reads_the_end_in_append_order_within_a_budget() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..200 {
            append(dir.path(), "exhaust", "h1", 1 << 20, json!({"kind": "bash", "i": i}))
                .unwrap();
        }
        let all = read_all(dir.path(), "exhaust");
        assert_eq!(all.records.len(), 200);
        let tail = read_tail(dir.path(), "exhaust", "h1", 600);
        assert!(tail.records.len() < 200, "the window is bounded: {}", tail.records.len());
        assert!(!tail.records.is_empty());
        let ids: Vec<i64> = tail.records.iter().map(|r| r.i64("i")).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted, "append order is preserved (the traps depend on it)");
        assert_eq!(*ids.last().unwrap(), 199, "the window ends at the newest record");
    }

    #[test]
    fn tail_spans_rotated_generations() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..40 {
            append(dir.path(), "exhaust", "h1", 500, json!({"kind": "bash", "i": i})).unwrap();
        }
        assert!(rotated_path(&stream_path(dir.path(), "exhaust", "h1"), 1).exists());
        let tail = read_tail(dir.path(), "exhaust", "h1", 1 << 20);
        let ids: Vec<i64> = tail.records.iter().map(|r| r.i64("i")).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted, "rotated generations come first, still in order");
        assert!(ids.len() > 5, "the tail crosses the rotation boundary: {ids:?}");
    }

    #[test]
    fn byte_counts_stay_informative_at_every_scale() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(240), "240 B", "a small stream must not read as '0 KiB'");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(32 * 1024 * 1024), "32.0 MiB");
    }

    #[test]
    fn missing_directory_reads_empty_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("nothing-here");
        let all = read_all(&absent, "exhaust");
        assert!(all.records.is_empty());
        assert!(all.unreadable.is_empty());
        assert_eq!(footprint(&absent, "exhaust"), 0);
        assert!(!absent.exists(), "a read must never create tree state");
    }
}
