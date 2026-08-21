//! The recall index: SQLite + FTS5 over the brain's markdown, rings 0-4.
//!
//! The DB is a per-host DERIVED, DISPOSABLE cache of the shared tree — it
//! lives in the local state dir (SQLite WAL cannot live on NFS), is rebuilt
//! whenever the tree's (path, mtime, size) set changes, and is deleted and
//! recreated on any corruption. The git-tracked markdown stays the only source
//! of truth.
//!
//! Citations are content-addressed: `r<ring>-<6 hex of sha256(normalized
//! block)>`. They survive reordering and unrelated edits; an edited entry
//! becomes a new citation by construction. The ring prefix makes the trust
//! level of a hit visible in the id itself.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use rusqlite::Connection;
use sha2::Digest as _;

use crate::resident::blank_private;

/// Rings 5-6 are never indexed: staging and exhaust must not surface in
/// recall. A file declaring itself ring 5+ is skipped entirely.
const MAX_INDEXED_RING: u8 = 4;

pub struct Block {
    pub cite: String,
    pub path: String,
    pub ring: u8,
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
}

#[derive(Debug)]
pub struct Hit {
    pub cite: String,
    pub path: String,
    pub ring: u8,
    pub start_line: usize,
    pub end_line: usize,
    pub snippet: String,
}

/// Location defaults for a brain-root-relative path; frontmatter `ring: N`
/// overrides. Mirrors DESIGN.md "existing brain -> ring mapping".
pub fn default_ring(rel: &str) -> u8 {
    if rel == "AGENT.md" || rel == "README.md" {
        1
    } else if rel.starts_with("mind/memories/") {
        // Exactly the index file — a topic file named e.g. OLD-MEMORY.md must
        // not inherit ring 1 by suffix accident.
        if rel == "mind/memories/MEMORY.md" { 1 } else { 2 }
    } else if rel.starts_with("todo/") {
        4
    } else {
        3
    }
}

/// Paths that must never enter the index. `knowledge/archive/` is excluded by
/// convention (do not load unless investigating history); secrets and logs are
/// excluded as a hard boundary; `projects/` holds repo clones (Milestone 3's
/// code index owns those).
fn excluded(rel: &str) -> bool {
    rel.starts_with("mind/secrets/")
        || rel.starts_with("logs/")
        || rel.starts_with("projects/")
        || rel.starts_with("knowledge/archive/")
        || rel.starts_with(".git/")
        || rel.contains("/.git/")
}

/// Secret-shaped file names are refused even outside mind/secrets/ — capture
/// the guard at the earliest point so every downstream store inherits it.
fn secret_shaped(rel: &str) -> bool {
    let base = rel.rsplit('/').next().unwrap_or(rel).to_ascii_lowercase();
    base.contains("secret")
        || base.contains("credential")
        || base.contains("password")
        || base.starts_with(".env")
        || base.ends_with(".pem")
        || base.ends_with(".key")
}

/// Parses a leading `---` frontmatter for `ring: N`. Returns (ring override,
/// line count of the frontmatter block including fences).
///
/// FAIL CLOSED: a `ring:` key whose value does not parse cleanly yields 255
/// (= skip the file). A malformed declaration on quarantined content must
/// never fall back to an indexable default.
fn frontmatter_ring(text: &str) -> (Option<u8>, usize) {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, 0);
    }
    let mut ring = None;
    for (i, line) in lines.enumerate() {
        let t = line.trim();
        if t == "---" {
            return (ring, i + 2);
        }
        let lower = t.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("ring:") {
            let token = v.split_whitespace().next().unwrap_or("");
            ring = Some(token.parse::<u8>().unwrap_or(255));
        }
    }
    // Unterminated frontmatter: treat as content.
    (None, 0)
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_lowercase()
}

/// Extracts `[[wikilink]]` target stems: alias (`|…`) and heading (`#…`)
/// parts dropped, lowercased. The brain is an Obsidian vault — these are
/// human-curated edges, the graph we trust most.
pub fn wikilinks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else { break };
        let inner = &after[..end];
        let target = inner.split(['|', '#']).next().unwrap_or("").trim();
        if !target.is_empty() && !target.contains('\n') {
            out.push(target.to_ascii_lowercase());
        }
        rest = &after[end + 2..];
    }
    out
}

/// 40 hash bits: at ~20k blocks the birthday collision expectation is ~0.0002
/// — the 24-bit version measurably collided in the real corpus.
pub fn cite_id(ring: u8, text: &str) -> String {
    let digest = sha2::Sha256::digest(normalize(text).as_bytes());
    format!(
        "r{ring}-{:02x}{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3], digest[4]
    )
}

/// Splits markdown into logical blocks: heading, list item (with indented
/// continuation), table row, fenced code block, or paragraph. Line numbers are
/// 1-indexed into the ORIGINAL file (the caller passes blanked text of equal
/// line structure).
pub fn segment(text: &str, skip_lines: usize) -> Vec<(usize, usize, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks = Vec::new();
    let mut i = skip_lines;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }
        let start = i;
        if trimmed.starts_with("```") {
            // CommonMark closing rule: only a run of AT LEAST the opening
            // length closes the fence — a ```` fence containing ``` examples
            // must not close early.
            let open_run = trimmed.chars().take_while(|&c| c == '`').count();
            i += 1;
            while i < lines.len() {
                let t = lines[i].trim_start();
                if t.chars().take_while(|&c| c == '`').count() >= open_run
                    && t.starts_with("```")
                {
                    break;
                }
                i += 1;
            }
            i = (i + 1).min(lines.len());
        } else if trimmed.starts_with('#') {
            i += 1;
        } else if trimmed.starts_with('|') {
            i += 1; // one table row per block: rows are independent statements
        } else if is_list_item(trimmed) {
            i += 1;
            while i < lines.len()
                && !lines[i].trim_start().is_empty()
                && lines[i].starts_with(' ')
                && !is_list_item(lines[i].trim_start())
            {
                i += 1;
            }
        } else {
            while i < lines.len() {
                let t = lines[i].trim_start();
                if t.is_empty() || t.starts_with('#') || t.starts_with('|') || is_list_item(t) || t.starts_with("```") {
                    break;
                }
                i += 1;
            }
        }
        let body = lines[start..i].join("\n");
        if !body.trim().is_empty() {
            blocks.push((start + 1, i, body));
        }
    }
    blocks
}

fn is_list_item(t: &str) -> bool {
    t.starts_with("- ")
        || t.starts_with("* ")
        || t.starts_with("+ ")
        || t.split_once('.')
            .is_some_and(|(n, rest)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) && rest.starts_with(' '))
}

fn db_path(state_dir: &Path) -> PathBuf {
    state_dir.join("index.db")
}

/// Bump whenever tables/columns/id formats change: an old DB with a new
/// binary is silently wrong (e.g. stale cite widths), and the cache is
/// disposable — mismatches are handled by delete-and-rebuild in `open()`.
const SCHEMA_VERSION: i64 = 5; // 5: rank_pct + import_edges + vectors (merged)

fn open_at(path: &Path) -> anyhow::Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_millis(1000))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != SCHEMA_VERSION {
        let tables: i64 =
            conn.query_row("SELECT count(*) FROM sqlite_master WHERE type='table'", [], |r| r.get(0))?;
        if tables > 0 {
            anyhow::bail!("index schema v{version} != v{SCHEMA_VERSION}; rebuild required");
        }
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS docs(
           id INTEGER PRIMARY KEY,
           path TEXT UNIQUE NOT NULL,
           ring INTEGER NOT NULL,
           mtime INTEGER NOT NULL,
           size INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS blocks(
           id INTEGER PRIMARY KEY,
           cite TEXT NOT NULL,
           doc_id INTEGER NOT NULL REFERENCES docs(id) ON DELETE CASCADE,
           start_line INTEGER NOT NULL,
           end_line INTEGER NOT NULL,
           text TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS blocks_cite ON blocks(cite);
         CREATE TABLE IF NOT EXISTS links(
           from_doc INTEGER NOT NULL REFERENCES docs(id) ON DELETE CASCADE,
           to_doc INTEGER NOT NULL REFERENCES docs(id) ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS links_from ON links(from_doc);
         CREATE INDEX IF NOT EXISTS links_to ON links(to_doc);
         CREATE VIRTUAL TABLE IF NOT EXISTS blocks_fts USING fts5(text, content='blocks', content_rowid='id');
         CREATE TABLE IF NOT EXISTS vectors(
           block_id INTEGER PRIMARY KEY REFERENCES blocks(id) ON DELETE CASCADE,
           embedding BLOB NOT NULL
         );",
    )?;
    Ok(conn)
}

/// Opens the index; a corrupt file is deleted and recreated (derived cache —
/// the tree is the truth).
pub fn open(state_dir: &Path) -> anyhow::Result<Connection> {
    let path = db_path(state_dir);
    match open_at(&path) {
        Ok(c) => Ok(c),
        Err(_) => {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(path.with_extension("db-wal"));
            let _ = std::fs::remove_file(path.with_extension("db-shm"));
            open_at(&path)
        }
    }
}

/// One indexable file: the doc path stored in the DB, where to read it, stat
/// info for staleness, and the ring to assume when no frontmatter overrides.
struct SourceFile {
    doc_path: String,
    abs: PathBuf,
    mtime: u64,
    size: u64,
    default_ring: u8,
}

fn stat_of(meta: &std::fs::Metadata) -> (u64, u64) {
    // Nanosecond precision: same-second same-size edits must flip staleness.
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    (mtime, meta.len())
}

/// Brain tree + (optionally) Claude Code's native auto-memory stores.
/// Native memory (`<native_root>/<project-slug>/memory/*.md`) is indexed as
/// ring 2 with doc paths `native:<slug>/<file>` — cfetch reads and surfaces
/// the native store, it never writes to it.
fn collect_files(brain_root: &Path, native_root: Option<&Path>) -> Vec<SourceFile> {
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(brain_root)
        .hidden(true)
        .git_ignore(true)
        .follow_links(false)
        .build();
    for entry in walker.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(brain_root) else { continue };
        let rel = rel.to_string_lossy().to_string();
        if !rel.ends_with(".md") || excluded(&rel) || secret_shaped(&rel) {
            continue;
        }
        let (mtime, size) = stat_of(&meta);
        out.push(SourceFile {
            doc_path: rel.clone(),
            abs: entry.path().to_path_buf(),
            mtime,
            size,
            default_ring: default_ring(&rel),
        });
    }
    if let Some(projects) = native_root.and_then(|nr| std::fs::read_dir(nr).ok()) {
        for project in projects.flatten() {
            let slug = project.file_name().to_string_lossy().to_string();
            let mem_dir = project.path().join("memory");
            let Ok(files) = std::fs::read_dir(&mem_dir) else { continue };
            for f in files.flatten() {
                let Ok(meta) = f.metadata() else { continue };
                let name = f.file_name().to_string_lossy().to_string();
                if !meta.is_file() || !name.ends_with(".md") || secret_shaped(&name) {
                    continue;
                }
                let (mtime, size) = stat_of(&meta);
                out.push(SourceFile {
                    doc_path: format!("native:{slug}/{name}"),
                    abs: f.path(),
                    mtime,
                    size,
                    default_ring: 2,
                });
            }
        }
    }
    out.sort_by(|a, b| a.doc_path.cmp(&b.doc_path));
    out
}

/// One value answering "does this index describe these sources": sha256 over
/// the sorted (doc_path, mtime, size) list — INCLUDING files the scan later
/// skips by ring, so a ring-frontmatter edit or a skipped file's change flips
/// staleness like any other.
fn source_fingerprint(files: &[SourceFile]) -> String {
    let mut hasher = sha2::Sha256::new();
    for f in files {
        hasher.update(f.doc_path.as_bytes());
        hasher.update([0u8]);
        hasher.update(f.mtime.to_le_bytes());
        hasher.update(f.size.to_le_bytes());
        hasher.update([0xffu8]);
    }
    format!("{:x}", hasher.finalize())
}

/// Cheap staleness decision: stat-only fingerprint comparison — no file
/// bodies are read.
pub fn stale(conn: &Connection, brain_root: &Path, native_root: Option<&Path>) -> anyhow::Result<bool> {
    let root_meta: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key='brain_root'", [], |r| r.get(0))
        .ok();
    if root_meta.as_deref() != Some(brain_root.to_string_lossy().as_ref()) {
        return Ok(true);
    }
    let stored: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key='source_fingerprint'", [], |r| r.get(0))
        .ok();
    let current = source_fingerprint(&collect_files(brain_root, native_root));
    Ok(stored.as_deref() != Some(current.as_str()))
}

pub struct ScanReport {
    pub docs: usize,
    pub blocks: usize,
    pub skipped_high_ring: usize,
}

/// Full rebuild inside one transaction. The corpus is small markdown; a
/// rebuild is cheaper and simpler than incremental sync, and matches the
/// disposable-cache design.
pub fn scan(conn: &mut Connection, brain_root: &Path, native_root: Option<&Path>) -> anyhow::Result<ScanReport> {
    let files = collect_files(brain_root, native_root);
    let fingerprint = source_fingerprint(&files);
    // Read every body BEFORE the write transaction: the tree may be NFS, and
    // holding SQLite's writer lock across slow I/O starves concurrent readers
    // into SQLITE_BUSY failures.
    let bodies: Vec<(SourceFile, String)> = files
        .into_iter()
        .filter_map(|src| std::fs::read_to_string(&src.abs).ok().map(|raw| (src, raw)))
        .collect();
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    // Vectors MUST go with the blocks they describe: after `DELETE FROM
    // blocks` SQLite reuses rowids from 1, so a stale vectors row would
    // silently attach to an unrelated new block. Embeddings are derived data;
    // `embed-index` rebuilds them resumably.
    tx.execute_batch(
        "DELETE FROM vectors; DELETE FROM blocks; DELETE FROM docs;
         INSERT INTO blocks_fts(blocks_fts) VALUES('delete-all');",
    )?;
    tx.execute(
        "INSERT INTO meta(key, value) VALUES('brain_root', ?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [brain_root.to_string_lossy().as_ref()],
    )?;
    tx.execute(
        "INSERT INTO meta(key, value) VALUES('source_fingerprint', ?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [fingerprint],
    )?;
    let mut report = ScanReport { docs: 0, blocks: 0, skipped_high_ring: 0 };
    // (doc_id, link stems) collected during insertion, resolved afterwards
    // when every doc is known.
    let mut pending_links: Vec<(i64, Vec<String>)> = Vec::new();
    for (src, raw) in bodies {
        let (fm_ring, fm_lines) = frontmatter_ring(&raw);
        let mut ring = fm_ring.unwrap_or(src.default_ring);
        // The native store's contract ring is 2: honor demotion to 5+ (skip),
        // never self-promotion into the resident/policy rings.
        if src.doc_path.starts_with("native:") {
            ring = ring.max(2);
        }
        if ring > MAX_INDEXED_RING {
            report.skipped_high_ring += 1;
            continue;
        }
        // Blank (not strip) private regions so line numbers stay accurate.
        let blanked = blank_private(&raw);
        tx.execute(
            "INSERT INTO docs(path, ring, mtime, size) VALUES(?1, ?2, ?3, ?4)",
            rusqlite::params![src.doc_path, ring, src.mtime as i64, src.size as i64],
        )?;
        let doc_id = tx.last_insert_rowid();
        let links = wikilinks(&blanked);
        if !links.is_empty() {
            pending_links.push((doc_id, links));
        }
        for (start, end, body) in segment(&blanked, fm_lines) {
            let cite = cite_id(ring, &body);
            tx.execute(
                "INSERT INTO blocks(cite, doc_id, start_line, end_line, text)
                 VALUES(?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![cite, doc_id, start as i64, end as i64, body],
            )?;
            let block_id = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO blocks_fts(rowid, text) VALUES(?1, ?2)",
                rusqlite::params![block_id, body],
            )?;
            report.blocks += 1;
        }
        report.docs += 1;
    }
    // Resolve link stems to doc ids: filename stem, unambiguous only —
    // ambiguous stems are skipped (mirrors brain-lint's path-qualify rule).
    {
        let mut by_stem: std::collections::HashMap<String, Option<i64>> = std::collections::HashMap::new();
        let mut stmt = tx.prepare("SELECT id, path FROM docs")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows.filter_map(Result::ok) {
            let (id, path) = row;
            let base = path.rsplit('/').next().unwrap_or(&path);
            let stem = base.strip_suffix(".md").unwrap_or(base).to_ascii_lowercase();
            by_stem
                .entry(stem)
                .and_modify(|e| *e = None) // ambiguous
                .or_insert(Some(id));
        }
        drop(stmt);
        for (from_doc, stems) in pending_links {
            for stem in stems {
                if let Some(Some(to_doc)) = by_stem.get(&stem)
                    && *to_doc != from_doc
                {
                    tx.execute(
                        "INSERT INTO links(from_doc, to_doc) VALUES(?1, ?2)",
                        rusqlite::params![from_doc, to_doc],
                    )?;
                }
            }
        }
    }
    tx.commit()?;
    Ok(report)
}

/// Docs linked (either direction, human-curated wikilinks) to the docs of the
/// given citation paths — the deterministic 1-hop graph expansion of a recall
/// result. Returns (path, ring) sorted by ring then path, deduped.
pub fn linked_docs(conn: &Connection, hit_paths: &[String], limit: usize) -> anyhow::Result<Vec<(String, u8)>> {
    let mut out: Vec<(String, u8)> = Vec::new();
    for path in hit_paths {
        let mut stmt = conn.prepare(
            "SELECT d2.path, d2.ring FROM docs d1
             JOIN links l ON l.from_doc = d1.id JOIN docs d2 ON d2.id = l.to_doc
             WHERE d1.path = ?1
             UNION
             SELECT d2.path, d2.ring FROM docs d1
             JOIN links l ON l.to_doc = d1.id JOIN docs d2 ON d2.id = l.from_doc
             WHERE d1.path = ?1",
        )?;
        let rows = stmt.query_map([path], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u8)))?;
        for row in rows.filter_map(Result::ok) {
            if !hit_paths.contains(&row.0) && !out.contains(&row) {
                out.push(row);
            }
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    out.truncate(limit);
    Ok(out)
}

/// FTS5 query string: each term becomes a quoted prefix token, OR-joined —
/// recall-heavy on purpose (precision gates come at the consumer).
fn fts_query(user_query: &str) -> String {
    user_query
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"*", t.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// One-line, length-capped preview of a block for hit listings.
fn snippet_of(text: &str) -> String {
    let one = text.split_whitespace().collect::<Vec<_>>().join(" ");
    one.chars().take(160).collect()
}

pub fn recall(conn: &Connection, query: &str, limit: usize) -> anyhow::Result<Vec<Hit>> {
    let fts = fts_query(query);
    if fts.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT b.cite, d.path, d.ring, b.start_line, b.end_line, b.text
         FROM blocks_fts f
         JOIN blocks b ON b.id = f.rowid
         JOIN docs d ON d.id = b.doc_id
         WHERE blocks_fts MATCH ?1
         ORDER BY bm25(blocks_fts), d.ring ASC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![fts, limit as i64], |r| {
        Ok(Hit {
            cite: r.get(0)?,
            path: r.get(1)?,
            ring: r.get::<_, i64>(2)? as u8,
            start_line: r.get::<_, i64>(3)? as usize,
            end_line: r.get::<_, i64>(4)? as usize,
            snippet: snippet_of(&r.get::<_, String>(5)?),
        })
    })?;
    Ok(rows.filter_map(Result::ok).collect())
}

/// Expands a citation id to its full block(s) — the second disclosure layer.
/// Content-addressing means an id can legitimately match in several files.
pub fn expand(conn: &Connection, cite: &str) -> anyhow::Result<Vec<Block>> {
    let mut stmt = conn.prepare(
        "SELECT b.cite, d.path, d.ring, b.start_line, b.end_line, b.text
         FROM blocks b JOIN docs d ON d.id = b.doc_id WHERE b.cite = ?1",
    )?;
    let rows = stmt.query_map([cite], |r| {
        Ok(Block {
            cite: r.get(0)?,
            path: r.get(1)?,
            ring: r.get::<_, i64>(2)? as u8,
            start_line: r.get::<_, i64>(3)? as usize,
            end_line: r.get::<_, i64>(4)? as usize,
            text: r.get(5)?,
        })
    })?;
    Ok(rows.filter_map(Result::ok).collect())
}

// ---- vector store (semantic + hybrid recall) ----
//
// Embeddings live INSIDE index.db as little-endian f32 blobs, one row per
// block, L2-normalized at insert so cosine similarity reduces to a dot
// product at query time. No vector-index dependency: at ~20k blocks a linear
// scan in Rust is milliseconds, exact, and zero-dep (the sanctioned fallback
// in DESIGN.md). A missing row means "not yet embedded" — that single fact
// makes `embed-index` resumable for free.

/// Little-endian f32 encoding — the on-disk vector format.
pub fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// Inverse of [`vec_to_blob`]; a trailing partial chunk (corrupt blob) is
/// dropped rather than misread.
pub fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    let (chunks, _remainder) = b.as_chunks::<4>();
    chunks.iter().map(|c| f32::from_le_bytes(*c)).collect()
}

/// L2-normalizes in place; the zero vector is left untouched (it can never
/// rank anyway).
pub fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Plain dot product — cosine similarity, given both sides are normalized.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn meta_get(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0)).ok()
}

fn meta_set(conn: &Connection, key: &str, value: &str) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [key, value],
    )?;
    Ok(())
}

/// Records the embedding model in meta; a DIFFERENT stored model drops every
/// vector (and the stored dimension) — mixed-model similarity is meaningless,
/// and rebuild is a first-class, resumable path. Returns true when vectors
/// were dropped.
pub fn ensure_embed_model(conn: &Connection, model: &str) -> anyhow::Result<bool> {
    let stored = meta_get(conn, "embed_model");
    if stored.as_deref() == Some(model) {
        return Ok(false);
    }
    let dropping = stored.is_some();
    conn.execute("DELETE FROM vectors", [])?;
    // The dimension belongs to the model: clear it so the next embed run
    // re-learns it from the first response.
    conn.execute("DELETE FROM meta WHERE key='embed_dim'", [])?;
    meta_set(conn, "embed_model", model)?;
    Ok(dropping)
}

/// Same contract as [`ensure_embed_model`] for the vector dimension (a model
/// NAME can silently change dimension when the endpoint is reconfigured).
pub fn ensure_embed_dim(conn: &Connection, dim: usize) -> anyhow::Result<bool> {
    let stored = meta_get(conn, "embed_dim");
    let dim_str = dim.to_string();
    if stored.as_deref() == Some(dim_str.as_str()) {
        return Ok(false);
    }
    let dropping = stored.is_some();
    conn.execute("DELETE FROM vectors", [])?;
    meta_set(conn, "embed_dim", &dim_str)?;
    Ok(dropping)
}

/// Blocks with no vector row yet, in stable id order — the embed-index work
/// queue. Missing row = not yet embedded (resumability contract).
pub fn blocks_without_vectors(conn: &Connection, limit: usize) -> anyhow::Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT b.id, b.text FROM blocks b
         LEFT JOIN vectors v ON v.block_id = b.id
         WHERE v.block_id IS NULL
         ORDER BY b.id LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    Ok(rows.filter_map(Result::ok).collect())
}

/// (embedded, total blocks) — progress reporting.
pub fn vector_counts(conn: &Connection) -> anyhow::Result<(usize, usize)> {
    let (v, b): (i64, i64) = conn.query_row(
        "SELECT (SELECT count(*) FROM vectors), (SELECT count(*) FROM blocks)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((v as usize, b as usize))
}

/// Stores one block's embedding, L2-normalized, as a little-endian f32 blob.
pub fn insert_vector(conn: &Connection, block_id: i64, embedding: &[f32]) -> anyhow::Result<()> {
    let mut v = embedding.to_vec();
    l2_normalize(&mut v);
    conn.execute(
        "INSERT INTO vectors(block_id, embedding) VALUES(?1, ?2)
         ON CONFLICT(block_id) DO UPDATE SET embedding=excluded.embedding",
        rusqlite::params![block_id, vec_to_blob(&v)],
    )?;
    Ok(())
}

/// Hits for the given block ids, preserving the ids' order. Ids that no
/// longer exist (index moved on) are silently skipped.
fn hits_for_block_ids(conn: &Connection, ids: &[i64]) -> anyhow::Result<Vec<Hit>> {
    let mut stmt = conn.prepare(
        "SELECT b.cite, d.path, d.ring, b.start_line, b.end_line, b.text
         FROM blocks b JOIN docs d ON d.id = b.doc_id WHERE b.id = ?1",
    )?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let hit = stmt.query_row([id], |r| {
            Ok(Hit {
                cite: r.get(0)?,
                path: r.get(1)?,
                ring: r.get::<_, i64>(2)? as u8,
                start_line: r.get::<_, i64>(3)? as usize,
                end_line: r.get::<_, i64>(4)? as usize,
                snippet: snippet_of(&r.get::<_, String>(5)?),
            })
        });
        if let Ok(hit) = hit {
            out.push(hit);
        }
    }
    Ok(out)
}

/// Block ids ranked by dot product against a normalized query vector —
/// a full linear scan, exact by construction. Rows whose dimension does not
/// match the query are skipped (transitional state during a model change).
fn semantic_block_ids(conn: &Connection, query_vec: &[f32], limit: usize) -> anyhow::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT block_id, embedding FROM vectors")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?;
    let mut scored: Vec<(i64, f32)> = rows
        .filter_map(Result::ok)
        .filter_map(|(id, blob)| {
            let v = blob_to_vec(&blob);
            (v.len() == query_vec.len()).then(|| (id, dot(&v, query_vec)))
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    scored.truncate(limit);
    Ok(scored.into_iter().map(|(id, _)| id).collect())
}

/// Pure cosine ranking over all stored vectors (query vector must be
/// normalized). Blocks without vectors simply cannot appear.
pub fn semantic_recall(conn: &Connection, query_vec: &[f32], limit: usize) -> anyhow::Result<Vec<Hit>> {
    let ids = semantic_block_ids(conn, query_vec, limit)?;
    hits_for_block_ids(conn, &ids)
}

/// BM25-ranked block ids for the same query shape [`recall`] uses.
fn bm25_block_ids(conn: &Connection, query: &str, limit: usize) -> anyhow::Result<Vec<i64>> {
    let fts = fts_query(query);
    if fts.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT b.id FROM blocks_fts f
         JOIN blocks b ON b.id = f.rowid
         JOIN docs d ON d.id = b.doc_id
         WHERE blocks_fts MATCH ?1
         ORDER BY bm25(blocks_fts), d.ring ASC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![fts, limit as i64], |r| r.get(0))?;
    Ok(rows.filter_map(Result::ok).collect())
}

/// Reciprocal rank fusion over ranked id lists: score(d) = Σ 1/(k + rank),
/// rank starting at 1. Ties break by id for determinism.
pub fn rrf_fuse(lists: &[Vec<i64>], k: f64) -> Vec<i64> {
    let mut score: std::collections::HashMap<i64, f64> = std::collections::HashMap::new();
    for list in lists {
        for (i, id) in list.iter().enumerate() {
            *score.entry(*id).or_default() += 1.0 / (k + (i + 1) as f64);
        }
    }
    let mut items: Vec<(i64, f64)> = score.into_iter().collect();
    items.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    items.into_iter().map(|(id, _)| id).collect()
}

/// BM25 list ⊕ semantic list via RRF — each fetched with a wider pool than
/// the final limit so fusion has something to reorder.
pub fn hybrid_recall(
    conn: &Connection,
    query: &str,
    query_vec: &[f32],
    limit: usize,
    rrf_k: f64,
) -> anyhow::Result<Vec<Hit>> {
    let pool = (limit * 4).max(20);
    let lexical = bm25_block_ids(conn, query, pool)?;
    let semantic = semantic_block_ids(conn, query_vec, pool)?;
    let mut fused = rrf_fuse(&[lexical, semantic], rrf_k);
    fused.truncate(limit);
    hits_for_block_ids(conn, &fused)
}

/// Ensures the index exists and is fresh, rebuilding when stale. Rebuilds are
/// serialized by a lockfile: when another process is already rebuilding, this
/// one serves the still-valid committed snapshot instead of failing with
/// SQLITE_BUSY or duplicating the work.
pub fn ensure_fresh(
    state_dir: &Path,
    brain_root: &Path,
    native_root: Option<&Path>,
) -> anyhow::Result<Connection> {
    let mut conn = open(state_dir).context("open index")?;
    if stale(&conn, brain_root, native_root)? {
        // `None` = another rebuilder is active; serve the committed snapshot.
        let lock = crate::lockfile::acquire(&state_dir.join("scan.lock"), 500, 120);
        // Re-check under the lock: the previous holder may have rebuilt
        // exactly what we were about to.
        if lock.is_some() && stale(&conn, brain_root, native_root)? {
            scan(&mut conn, brain_root, native_root)?;
        }
    }
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brain(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (rel, content) in files {
            let p = dir.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        dir
    }

    #[test]
    fn ring_defaults_follow_design_mapping() {
        assert_eq!(default_ring("AGENT.md"), 1);
        assert_eq!(default_ring("mind/memories/MEMORY.md"), 1);
        assert_eq!(default_ring("mind/memories/feedback_x.md"), 2);
        assert_eq!(default_ring("knowledge/hosts/server/zfs.md"), 3);
        assert_eq!(default_ring("todo/active/cfetch/STATUS.md"), 4);
    }

    #[test]
    fn citation_is_stable_under_reorder_and_case() {
        let a = cite_id(3, "The  Quick   Fox");
        let b = cite_id(3, "the quick fox");
        assert_eq!(a, b);
        assert!(a.starts_with("r3-"));
        assert_ne!(a, cite_id(3, "the quick foxes"));
        assert_ne!(a, cite_id(2, "the quick fox"), "ring is part of the id");
    }

    #[test]
    fn segmentation_splits_list_items_and_tables() {
        let text = "# H\n\n- item one\n  continued\n- item two\n\n| a | b |\n| c | d |\n\npara one\npara two\n";
        let blocks = segment(text, 0);
        let bodies: Vec<&str> = blocks.iter().map(|(_, _, b)| b.as_str()).collect();
        assert!(bodies.contains(&"# H"));
        assert!(bodies.contains(&"- item one\n  continued"));
        assert!(bodies.contains(&"- item two"));
        assert!(bodies.contains(&"| a | b |"));
        assert!(bodies.contains(&"para one\npara two"));
        let (start, end, _) = blocks.iter().find(|(_, _, b)| b.starts_with("- item one")).unwrap();
        assert_eq!((*start, *end), (3, 4));
    }

    #[test]
    fn scan_and_recall_end_to_end() {
        let dir = brain(&[
            ("AGENT.md", "# Rules\n\n- never rsync zfs to zfs\n"),
            ("knowledge/world/opentofu.md", "OpenTofu state is AES encrypted.\n\nPulumi is retired.\n"),
            ("todo/active/x/STATUS.md", "current quest: ship the recall index\n"),
        ]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        let report = scan(&mut conn, dir.path(), None).unwrap();
        assert_eq!(report.docs, 3);
        assert!(report.blocks >= 4);

        let hits = recall(&conn, "pulumi", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].ring, 3);
        assert!(hits[0].cite.starts_with("r3-"));
        assert!(hits[0].path.ends_with("opentofu.md"));

        // prefix matching: "encrypt" hits "encrypted"
        let hits = recall(&conn, "encrypt", 5).unwrap();
        assert_eq!(hits.len(), 1);

        let full = expand(&conn, &hits[0].cite).unwrap();
        assert_eq!(full.len(), 1);
        assert!(full[0].text.contains("AES encrypted"));
    }

    #[test]
    fn private_content_never_enters_the_index() {
        let dir = brain(&[(
            "knowledge/x.md",
            "public fact\n\n<private>hunter2 is the password</private>\n\nanother fact\n",
        )]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, dir.path(), None).unwrap();
        assert!(recall(&conn, "hunter2", 5).unwrap().is_empty());
        assert_eq!(recall(&conn, "public", 5).unwrap().len(), 1);
    }

    #[test]
    fn frontmatter_ring_overrides_and_high_rings_are_skipped() {
        let dir = brain(&[
            ("knowledge/promoted.md", "---\nring: 1\n---\nlocked decision here\n"),
            ("knowledge/staged.md", "---\nring: 5\n---\nquarantined candidate\n"),
        ]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        let report = scan(&mut conn, dir.path(), None).unwrap();
        assert_eq!(report.docs, 1);
        assert_eq!(report.skipped_high_ring, 1);
        let hits = recall(&conn, "locked decision", 5).unwrap();
        assert_eq!(hits[0].ring, 1);
        assert!(recall(&conn, "quarantined", 5).unwrap().is_empty());
    }

    #[test]
    fn secrets_and_archive_are_excluded() {
        let dir = brain(&[
            ("mind/secrets/README.md", "index of all tokens\n"),
            ("knowledge/archive/old.md", "retired design\n"),
            ("knowledge/live.md", "live fact\n"),
        ]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        let report = scan(&mut conn, dir.path(), None).unwrap();
        assert_eq!(report.docs, 1);
        assert!(recall(&conn, "tokens", 5).unwrap().is_empty());
        assert!(recall(&conn, "retired", 5).unwrap().is_empty());
    }

    #[test]
    fn native_auto_memory_is_indexed_as_ring2() {
        let brain = brain(&[("knowledge/a.md", "brain fact\n")]);
        let native = tempfile::tempdir().unwrap();
        let mem = native.path().join("-home-user/memory");
        std::fs::create_dir_all(&mem).unwrap();
        std::fs::write(mem.join("MEMORY.md"), "# Memory\n\n- [zvol trap](f.md) nossd required\n").unwrap();
        std::fs::write(
            mem.join("feedback_zvol.md"),
            "---\nname: feedback_zvol\ndescription: x\n---\nzvol on btrfs needs nossd mount option\n",
        )
        .unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        let report = scan(&mut conn, brain.path(), Some(native.path())).unwrap();
        assert_eq!(report.docs, 3);
        let hits = recall(&conn, "nossd", 5).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.ring == 2));
        assert!(hits.iter().any(|h| h.path == "native:-home-user/feedback_zvol.md"));
        // frontmatter of native files has no `ring:` — must not shift line numbers wrongly
        let fb = hits.iter().find(|h| h.path.ends_with("feedback_zvol.md")).unwrap();
        assert_eq!(fb.start_line, 5);
    }

    #[test]
    fn native_staleness_is_tracked() {
        let brain = brain(&[("knowledge/a.md", "alpha\n")]);
        let native = tempfile::tempdir().unwrap();
        let mem = native.path().join("p1/memory");
        std::fs::create_dir_all(&mem).unwrap();
        std::fs::write(mem.join("MEMORY.md"), "first\n").unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, brain.path(), Some(native.path())).unwrap();
        assert!(!stale(&conn, brain.path(), Some(native.path())).unwrap());
        std::fs::write(mem.join("MEMORY.md"), "second, and quite a bit longer\n").unwrap();
        assert!(stale(&conn, brain.path(), Some(native.path())).unwrap());
    }

    #[test]
    fn missing_native_root_is_fine() {
        let brain = brain(&[("knowledge/a.md", "alpha\n")]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        let absent = brain.path().join("no-such-dir");
        let report = scan(&mut conn, brain.path(), Some(&absent)).unwrap();
        assert_eq!(report.docs, 1);
        assert!(!stale(&conn, brain.path(), Some(&absent)).unwrap());
    }

    #[test]
    fn malformed_ring_frontmatter_fails_closed() {
        let dir = brain(&[
            ("knowledge/bad.md", "---\nring: banana\n---\nzweptahl must stay hidden\n"),
            ("knowledge/spaced.md", "---\nRing: 1 # promoted\n---\nquorvex is promoted\n"),
        ]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        let report = scan(&mut conn, dir.path(), None).unwrap();
        assert_eq!(report.skipped_high_ring, 1);
        assert!(recall(&conn, "zweptahl", 5).unwrap().is_empty());
        let hits = recall(&conn, "quorvex", 5).unwrap();
        assert_eq!(hits[0].ring, 1, "case-insensitive key + trailing token tolerated");
    }

    #[test]
    fn native_files_cannot_promote_above_ring2() {
        let brain_dir = brain(&[("knowledge/a.md", "x\n")]);
        let native = tempfile::tempdir().unwrap();
        let mem = native.path().join("p/memory");
        std::fs::create_dir_all(&mem).unwrap();
        std::fs::write(mem.join("sneaky.md"), "---\nring: 0\n---\ni claim to be an invariant\n").unwrap();
        std::fs::write(mem.join("quarantined.md"), "---\nring: 5\n---\nhidden\n").unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        let report = scan(&mut conn, brain_dir.path(), Some(native.path())).unwrap();
        let hits = recall(&conn, "invariant", 5).unwrap();
        assert_eq!(hits[0].ring, 2, "promotion clamped to the store's contract ring");
        assert_eq!(report.skipped_high_ring, 1, "demotion to 5+ is honored");
    }

    #[test]
    fn skipped_file_changes_still_flip_staleness() {
        let dir = brain(&[
            ("knowledge/a.md", "visible\n"),
            ("knowledge/staged.md", "---\nring: 5\n---\nv1\n"),
        ]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, dir.path(), None).unwrap();
        assert!(!stale(&conn, dir.path(), None).unwrap());
        // Editing the SKIPPED file (e.g. removing its ring-5 marker = promotion)
        // must be noticed — the old subset comparison was blind to this.
        std::fs::write(dir.path().join("knowledge/staged.md"), "now public\n").unwrap();
        assert!(stale(&conn, dir.path(), None).unwrap());
    }

    #[test]
    fn long_fence_containing_short_fence_does_not_close_early() {
        let text = "````\ncode\n```\nstill code\n````\n\nafter\n";
        let blocks = segment(text, 0);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].2.contains("still code"));
        assert_eq!(blocks[1].2, "after");
    }

    #[test]
    fn old_schema_version_triggers_rebuild_not_silent_reuse() {
        let state = tempfile::tempdir().unwrap();
        {
            let conn = open(state.path()).unwrap();
            conn.execute("INSERT INTO meta(key,value) VALUES('marker','old')", []).unwrap();
            conn.pragma_update(None, "user_version", 1i64).unwrap();
        }
        let conn = open(state.path()).unwrap();
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let marker: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key='marker'", [], |r| r.get(0))
            .ok();
        assert!(marker.is_none(), "old-schema DB must be discarded, not reused");
    }

    #[test]
    fn wikilink_extraction_handles_alias_and_anchor() {
        let links = wikilinks("see [[Zfs-Dataset|the dataset doc]] and [[shares#SMB]] but not [[]]");
        assert_eq!(links, vec!["zfs-dataset", "shares"]);
    }

    #[test]
    fn recall_expansion_follows_curated_links_both_directions() {
        let dir = brain(&[
            ("knowledge/zfs.md", "pools and datasets, see [[shares]]\n"),
            ("knowledge/hosts/shares.md", "SMB share layout\n"),
            ("knowledge/backup.md", "backups mirror [[zfs]] snapshots\n"),
            ("knowledge/unrelated.md", "nothing here\n"),
        ]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, dir.path(), None).unwrap();
        let linked = linked_docs(&conn, &["knowledge/zfs.md".to_string()], 8).unwrap();
        let paths: Vec<&str> = linked.iter().map(|(p, _)| p.as_str()).collect();
        assert!(paths.contains(&"knowledge/hosts/shares.md"), "outgoing link followed");
        assert!(paths.contains(&"knowledge/backup.md"), "incoming link followed");
        assert!(!paths.contains(&"knowledge/unrelated.md"));
    }

    #[test]
    fn ambiguous_wikilink_stems_are_skipped() {
        let dir = brain(&[
            ("knowledge/a/readme2.md", "x\n"),
            ("knowledge/b/readme2.md", "y\n"),
            ("knowledge/linker.md", "see [[readme2]]\n"),
        ]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, dir.path(), None).unwrap();
        let linked = linked_docs(&conn, &["knowledge/linker.md".to_string()], 8).unwrap();
        assert!(linked.is_empty(), "ambiguous stem must create no edge");
    }

    #[test]
    fn f32_blob_roundtrip_is_little_endian() {
        let v = vec![0.25f32, -1.5, 3.0];
        let blob = vec_to_blob(&v);
        assert_eq!(blob.len(), 12);
        assert_eq!(&blob[0..4], &0.25f32.to_le_bytes());
        assert_eq!(blob_to_vec(&blob), v);
        // corrupt trailing partial chunk is dropped, not misread
        assert_eq!(blob_to_vec(&blob[..10]), vec![0.25f32, -1.5]);
    }

    #[test]
    fn cosine_on_known_vectors() {
        let mut a = vec![3.0f32, 4.0];
        l2_normalize(&mut a);
        assert!((a[0] - 0.6).abs() < 1e-6);
        assert!((a[1] - 0.8).abs() < 1e-6);
        assert!((dot(&a, &a) - 1.0).abs() < 1e-6);
        let mut x = vec![1.0f32, 0.0];
        let mut y = vec![0.0f32, 1.0];
        l2_normalize(&mut x);
        l2_normalize(&mut y);
        assert!(dot(&x, &y).abs() < 1e-6, "orthogonal vectors have cosine 0");
        let mut d = vec![1.0f32, 1.0];
        l2_normalize(&mut d);
        assert!((dot(&d, &x) - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
        // zero vector: normalization must not divide by zero
        let mut z = vec![0.0f32, 0.0];
        l2_normalize(&mut z);
        assert_eq!(z, vec![0.0, 0.0]);
    }

    #[test]
    fn rrf_fusion_on_hand_built_rankings_k2() {
        // list 1: a b c   list 2: c a d
        // K=2 scores: a = 1/3 + 1/4 = 0.583, c = 1/5 + 1/3 = 0.533,
        //             b = 1/4 = 0.25,        d = 1/5 = 0.2
        let fused = rrf_fuse(&[vec![1, 2, 3], vec![3, 1, 4]], 2.0);
        assert_eq!(fused, vec![1, 3, 2, 4]);
    }

    #[test]
    fn rrf_ties_break_by_id_deterministically() {
        // two disjoint single-item lists: equal scores, id order decides
        let fused = rrf_fuse(&[vec![9], vec![4]], 2.0);
        assert_eq!(fused, vec![4, 9]);
    }

    #[test]
    fn insert_normalizes_and_roundtrips_through_db() {
        let dir = brain(&[("knowledge/a.md", "- one\n")]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, dir.path(), None).unwrap();
        let missing = blocks_without_vectors(&conn, 10).unwrap();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].1, "- one");
        insert_vector(&conn, missing[0].0, &[3.0, 4.0]).unwrap();
        let blob: Vec<u8> = conn
            .query_row("SELECT embedding FROM vectors WHERE block_id=?1", [missing[0].0], |r| r.get(0))
            .unwrap();
        let stored = blob_to_vec(&blob);
        assert!((stored[0] - 0.6).abs() < 1e-6, "normalized at insert");
        assert!((stored[1] - 0.8).abs() < 1e-6);
        assert!(blocks_without_vectors(&conn, 10).unwrap().is_empty());
        assert_eq!(vector_counts(&conn).unwrap(), (1, 1));
    }

    #[test]
    fn model_change_drops_vectors() {
        let dir = brain(&[("knowledge/a.md", "- one\n- two\n")]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, dir.path(), None).unwrap();
        assert!(!ensure_embed_model(&conn, "nomic-v1").unwrap(), "first model: nothing to drop");
        assert!(!ensure_embed_dim(&conn, 2).unwrap());
        for (id, _) in blocks_without_vectors(&conn, 10).unwrap() {
            insert_vector(&conn, id, &[1.0, 0.0]).unwrap();
        }
        assert_eq!(vector_counts(&conn).unwrap().0, 2);
        assert!(!ensure_embed_model(&conn, "nomic-v1").unwrap(), "same model keeps vectors");
        assert_eq!(vector_counts(&conn).unwrap().0, 2);
        assert!(ensure_embed_model(&conn, "nomic-v2").unwrap(), "model change drops");
        assert_eq!(vector_counts(&conn).unwrap().0, 0);
        assert_eq!(blocks_without_vectors(&conn, 10).unwrap().len(), 2, "rebuild is first-class");
        // dimension change behaves the same way
        assert!(!ensure_embed_dim(&conn, 4).unwrap(), "dim was cleared by the model change");
        for (id, _) in blocks_without_vectors(&conn, 10).unwrap() {
            insert_vector(&conn, id, &[1.0, 0.0, 0.0, 0.0]).unwrap();
        }
        assert!(ensure_embed_dim(&conn, 8).unwrap(), "dim change drops");
        assert_eq!(vector_counts(&conn).unwrap().0, 0);
    }

    #[test]
    fn rescan_drops_vectors_because_rowids_are_reused() {
        let dir = brain(&[("knowledge/a.md", "- one\n- two\n")]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, dir.path(), None).unwrap();
        for (id, _) in blocks_without_vectors(&conn, 10).unwrap() {
            insert_vector(&conn, id, &[1.0, 0.0]).unwrap();
        }
        assert_eq!(vector_counts(&conn).unwrap().0, 2);
        scan(&mut conn, dir.path(), None).unwrap();
        assert_eq!(vector_counts(&conn).unwrap().0, 0, "full rebuild must clear derived vectors");
    }

    #[test]
    fn semantic_recall_ranks_by_cosine() {
        let dir = brain(&[
            ("knowledge/zfs.md", "pools and datasets\n"),
            ("knowledge/mail.md", "stalwart smtp\n"),
            ("knowledge/unembedded.md", "no vector here\n"),
        ]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, dir.path(), None).unwrap();
        let assign = |text_frag: &str, v: &[f32]| {
            let id: i64 = conn
                .query_row(
                    "SELECT id FROM blocks WHERE text LIKE '%' || ?1 || '%'",
                    [text_frag],
                    |r| r.get(0),
                )
                .unwrap();
            insert_vector(&conn, id, v).unwrap();
        };
        assign("pools", &[1.0, 0.0]);
        assign("stalwart", &[0.0, 1.0]);
        let mut q = vec![0.9f32, 0.1];
        l2_normalize(&mut q);
        let hits = semantic_recall(&conn, &q, 10).unwrap();
        assert_eq!(hits.len(), 2, "unembedded block cannot appear");
        assert!(hits[0].path.ends_with("zfs.md"), "closest vector first");
        assert!(hits[1].path.ends_with("mail.md"));
        assert!(hits[0].cite.starts_with("r3-"), "hits carry the normal citation shape");
        // limit applies
        assert_eq!(semantic_recall(&conn, &q, 1).unwrap().len(), 1);
    }

    #[test]
    fn hybrid_recall_fuses_lexical_and_semantic() {
        // "zfs" appears lexically only in doc A; doc B is only semantically
        // close to the query vector. Hybrid must surface both, lexical hit
        // first (it leads on BOTH its list rank and the fused score here).
        let dir = brain(&[
            ("knowledge/a.md", "zfs pools and datasets\n"),
            ("knowledge/b.md", "storage volumes explained\n"),
        ]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, dir.path(), None).unwrap();
        let mut stmt = conn.prepare("SELECT id FROM blocks ORDER BY id").unwrap();
        let ids: Vec<i64> = stmt.query_map([], |r| r.get(0)).unwrap().filter_map(Result::ok).collect();
        drop(stmt);
        insert_vector(&conn, ids[0], &[1.0, 0.0]).unwrap(); // a.md: far from query
        insert_vector(&conn, ids[1], &[0.0, 1.0]).unwrap(); // b.md: close to query
        let q = vec![0.0f32, 1.0];
        let hits = hybrid_recall(&conn, "zfs", &q, 10, 2.0).unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert!(paths.contains(&"knowledge/a.md"), "lexical-only hit present");
        assert!(paths.contains(&"knowledge/b.md"), "semantic-only hit present");
        // a.md: rank 1 lexical (1/3) + rank 2 semantic (1/4) = 0.583
        // b.md: rank 1 semantic (1/3) = 0.333
        assert_eq!(paths[0], "knowledge/a.md");
    }

    #[test]
    fn staleness_flips_on_edit_and_rescan_clears_it() {
        let dir = brain(&[("knowledge/a.md", "alpha\n")]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, dir.path(), None).unwrap();
        assert!(!stale(&conn, dir.path(), None).unwrap());
        std::fs::write(dir.path().join("knowledge/a.md"), "alpha beta, much longer now\n").unwrap();
        assert!(stale(&conn, dir.path(), None).unwrap());
        scan(&mut conn, dir.path(), None).unwrap();
        assert!(!stale(&conn, dir.path(), None).unwrap());
        assert_eq!(recall(&conn, "beta", 5).unwrap().len(), 1);
    }
}
