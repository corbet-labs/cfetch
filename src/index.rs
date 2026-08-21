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

use crate::config::RingRules;
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
    /// Paths of suppressed duplicate copies of this logical block: same
    /// content hash AND same heading chain on a higher (or equal) ring —
    /// e.g. the native auto-memory mirror of a brain file. Identical short
    /// blocks under DIFFERENT sections are different statements, never
    /// mirrors. Empty for a block that exists in exactly one place.
    pub mirrors: Vec<String>,
    /// Enclosing heading chain ("H1 > H2"), the second half of the mirror
    /// dedup key. Internal — the snippet already displays it.
    pub(crate) chain: String,
}

/// Rewrites `sep` to `/` — the canonical separator of every brain-relative
/// doc path.
///
/// Doc paths are matched by PREFIX against `mind/secrets/`, `logs/`,
/// `projects/`, `knowledge/archive/` and `.git/`, are stored in the catalog
/// and are printed in citations. A platform whose separator is `\` produces
/// `mind\secrets\age.key` from the same file, which matches none of those
/// prefixes — the secrets boundary would open by pure string accident. On a
/// platform whose separator is already `/`, a backslash is an ordinary
/// filename character and is left untouched.
pub(crate) fn normalize_separators(rel: &str, sep: char) -> String {
    if sep == '/' { rel.to_string() } else { rel.replace(sep, "/") }
}

/// Canonical doc path for a brain-root-relative path, on any platform.
pub(crate) fn rel_doc_path(rel: &Path) -> String {
    normalize_separators(&rel.to_string_lossy(), std::path::MAIN_SEPARATOR)
}

/// THE taxonomy entry point: the configured location default for a
/// brain-root-relative path. Frontmatter `ring: N` still overrides it at
/// scan time. Everything that needs a path's ring — the scan, the watcher,
/// ring-6 capture — comes through here, so the mapping lives in the config
/// and nowhere else.
pub fn default_ring(rel: &str, rules: &RingRules) -> u8 {
    rules.ring_for(rel)
}

/// Paths that must never enter the index: the compiled-in boundary (secrets,
/// logs, git internals) plus the operator's `exclude_prefixes` — by default
/// `projects/` (repo clones, owned by the code index) and
/// `knowledge/archive/` (retired knowledge, not recallable by accident).
fn excluded(rel: &str, rules: &RingRules) -> bool {
    rules.excluded(rel)
}

/// Directory form of [`excluded`]: true when nothing under `rel` can ever be
/// indexed, so the serving watcher can skip the whole subtree. One predicate
/// serves both forms — the watch set is the index set by construction, never
/// by two hand-maintained lists agreeing.
pub(crate) fn excluded_dir(rel: &str, rules: &RingRules) -> bool {
    rules.excluded_dir(rel)
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
pub(crate) fn frontmatter_ring(text: &str) -> (Option<u8>, usize) {
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

/// Extracts `[[wikilink]]` targets: alias (`|…`) and heading (`#…`) parts
/// dropped, lowercased; slash-qualified targets (`[[hosts/zfs]]`) survive
/// whole. Fenced code blocks are skipped first (brain-lint parity): a
/// `[[link]]` inside a ``` or ~~~ fence is an example, not an edge. The brain
/// is an Obsidian vault — these are human-curated edges, the graph we trust
/// most.
pub fn wikilinks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    for line in text.lines() {
        let t = line.trim_start();
        match fence {
            Some(open) => {
                if fence_closes(t, open) {
                    fence = None;
                }
                continue;
            }
            None => {
                if let Some(open) = fence_open(t) {
                    fence = Some(open);
                    continue;
                }
            }
        }
        let mut rest = line;
        while let Some(start) = rest.find("[[") {
            let after = &rest[start + 2..];
            let Some(end) = after.find("]]") else { break };
            let target = after[..end].split(['|', '#']).next().unwrap_or("").trim();
            if !target.is_empty() {
                out.push(target.to_ascii_lowercase());
            }
            rest = &after[end + 2..];
        }
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

/// A fence opener: a run of at least three backticks or tildes (CommonMark
/// treats both characters as code fences). Returns (fence char, run length).
fn fence_open(t: &str) -> Option<(char, usize)> {
    let c = t.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let run = t.chars().take_while(|&x| x == c).count();
    (run >= 3).then_some((c, run))
}

/// CommonMark closing rule: only a run of the SAME character at least as long
/// as the opener closes a fence — a ```` fence containing ``` examples (or a
/// ~~~ fence containing ```) must not close early.
fn fence_closes(t: &str, (ch, len): (char, usize)) -> bool {
    t.chars().take_while(|&x| x == ch).count() >= len
}

/// Setext heading underline: a line of only `=` (level 1) or only `-`
/// (level 2). Two characters minimum — a lone `-` in running text is far more
/// often a stray bullet than an underline.
fn setext_level(t: &str) -> Option<u8> {
    let t = t.trim();
    if t.len() >= 2 && t.chars().all(|c| c == '=') {
        Some(1)
    } else if t.len() >= 2 && t.chars().all(|c| c == '-') {
        Some(2)
    } else {
        None
    }
}

/// Continuation indentation: spaces or tabs both indent.
fn indented(line: &str) -> bool {
    line.starts_with(' ') || line.starts_with('\t')
}

/// Heading level and text of a block when the block IS a heading: ATX
/// (`## title`, closing hashes tolerated) or setext (text + `===`/`---`
/// underline as produced by [`segment`]).
fn heading_of(body: &str) -> Option<(u8, String)> {
    let mut it = body.lines();
    let first = it.next()?.trim_start();
    if first.starts_with('#') {
        let level = first.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&level) && it.next().is_none() {
            let text = first[level..].trim().trim_end_matches('#').trim();
            return Some((level as u8, text.to_string()));
        }
        return None;
    }
    let second = it.next()?;
    if it.next().is_none()
        && let Some(level) = setext_level(second)
    {
        return Some((level, first.trim().to_string()));
    }
    None
}

/// Splits markdown into logical blocks: heading (ATX or setext underline),
/// list item (with indented continuation, including blank-separated indented
/// continuation paragraphs), table row, fenced code block (``` or ~~~), or
/// paragraph. Line numbers are 1-indexed into the ORIGINAL file (the caller
/// passes blanked text of equal line structure).
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
        if let Some(open) = fence_open(trimmed) {
            i += 1;
            while i < lines.len() && !fence_closes(lines[i].trim_start(), open) {
                i += 1;
            }
            i = (i + 1).min(lines.len());
        } else if trimmed.starts_with('#') {
            i += 1;
        } else if trimmed.starts_with('|') {
            i += 1; // one table row per block: rows are independent statements
        } else if is_list_item(trimmed) {
            i += 1;
            loop {
                // Tight continuation: indented, non-blank, not itself an item.
                while i < lines.len()
                    && !lines[i].trim_start().is_empty()
                    && indented(lines[i])
                    && !is_list_item(lines[i].trim_start())
                {
                    i += 1;
                }
                // Loose continuation: blank line(s) followed by an indented
                // non-item line — an indented continuation paragraph still
                // belongs to the item (numbered items especially are written
                // this way).
                let mut j = i;
                while j < lines.len() && lines[j].trim().is_empty() {
                    j += 1;
                }
                if j > i
                    && j < lines.len()
                    && indented(lines[j])
                    && !is_list_item(lines[j].trim_start())
                {
                    i = j + 1;
                } else {
                    break;
                }
            }
        } else if i + 1 < lines.len() && setext_level(lines[i + 1]).is_some() {
            i += 2; // setext heading: the text line plus its underline
        } else {
            i += 1;
            while i < lines.len() {
                let t = lines[i].trim_start();
                if t.is_empty()
                    || t.starts_with('#')
                    || t.starts_with('|')
                    || is_list_item(t)
                    || fence_open(t).is_some()
                {
                    break;
                }
                // The upcoming line underlines THIS line into a setext
                // heading: the heading starts its own block.
                if i + 1 < lines.len() && setext_level(lines[i + 1]).is_some() {
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
    let numbered = |sep: char| {
        t.split_once(sep).is_some_and(|(n, rest)| {
            !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) && rest.starts_with(' ')
        })
    };
    t.starts_with("- ")
        || t.starts_with("* ")
        || t.starts_with("+ ")
        || numbered('.')
        || numbered(')')
}

fn db_path(state_dir: &Path) -> PathBuf {
    state_dir.join("index.db")
}

/// Bump whenever tables/columns/id formats change: an old DB with a new
/// binary is silently wrong (e.g. stale cite widths), and the cache is
/// disposable — mismatches are handled by delete-and-rebuild in `open()`.
const SCHEMA_VERSION: i64 = 6; // 6: heading-chain ctx, doc_links/skipped_docs, code norm columns

fn open_at(path: &Path) -> anyhow::Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_millis(1000))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // The ON DELETE CASCADE clauses below (and in code.rs) are load-bearing:
    // enforce them explicitly instead of relying on the bundled build's
    // compile-time default.
    conn.pragma_update(None, "foreign_keys", true)?;
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
           text TEXT NOT NULL,
           ctx TEXT NOT NULL DEFAULT '',
           chain TEXT NOT NULL DEFAULT ''
         );
         CREATE INDEX IF NOT EXISTS blocks_cite ON blocks(cite);
         CREATE INDEX IF NOT EXISTS blocks_doc ON blocks(doc_id);
         CREATE TABLE IF NOT EXISTS links(
           from_doc INTEGER NOT NULL REFERENCES docs(id) ON DELETE CASCADE,
           to_doc INTEGER NOT NULL REFERENCES docs(id) ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS links_from ON links(from_doc);
         CREATE INDEX IF NOT EXISTS links_to ON links(to_doc);
         CREATE TABLE IF NOT EXISTS doc_links(
           doc_id INTEGER NOT NULL REFERENCES docs(id) ON DELETE CASCADE,
           target TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS doc_links_doc ON doc_links(doc_id);
         CREATE TABLE IF NOT EXISTS skipped_docs(
           path TEXT PRIMARY KEY,
           mtime INTEGER NOT NULL,
           size INTEGER NOT NULL
         );
         CREATE VIRTUAL TABLE IF NOT EXISTS blocks_fts USING fts5(text, ctx, content='blocks', content_rowid='id');
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
fn collect_files(
    brain_root: &Path,
    native_root: Option<&Path>,
    rules: &RingRules,
) -> Vec<SourceFile> {
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
        let rel = rel_doc_path(rel);
        if !rel.ends_with(".md") || excluded(&rel, rules) || secret_shaped(&rel) {
            continue;
        }
        let (mtime, size) = stat_of(&meta);
        out.push(SourceFile {
            doc_path: rel.clone(),
            abs: entry.path().to_path_buf(),
            mtime,
            size,
            default_ring: default_ring(&rel, rules),
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
pub fn stale(
    conn: &Connection,
    brain_root: &Path,
    native_root: Option<&Path>,
    rules: &RingRules,
) -> anyhow::Result<bool> {
    let root_meta: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key='brain_root'", [], |r| r.get(0))
        .ok();
    if root_meta.as_deref() != Some(brain_root.to_string_lossy().as_ref()) {
        return Ok(true);
    }
    let stored: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key='source_fingerprint'", [], |r| r.get(0))
        .ok();
    let current = source_fingerprint(&collect_files(brain_root, native_root, rules));
    Ok(stored.as_deref() != Some(current.as_str()))
}

pub struct ScanReport {
    pub docs: usize,
    pub blocks: usize,
    pub skipped_high_ring: usize,
    /// Doc paths of the files skipped as ring 5+ — including fail-closed
    /// unparseable ring frontmatter — surfaced so a file quarantined by
    /// accident is visible instead of silently absent from recall.
    pub skipped: Vec<String>,
    /// Catalog generation this scan committed (see [`generation`]).
    pub generation: u64,
}

/// Joined heading texts of a chain, empty texts (blanked private headings)
/// skipped.
fn chain_text(chain: &[(u8, String)]) -> String {
    chain
        .iter()
        .map(|(_, t)| t.as_str())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" > ")
}

/// Inserts one source file's rows — doc, blocks (with heading-chain context),
/// FTS rows, wikilink targets — or records it in `skipped_docs` when its ring
/// is 5+. Shared by the full scan and the incremental rescan so both derive
/// byte-identical catalogs from the same tree.
fn insert_doc(
    tx: &rusqlite::Transaction<'_>,
    src: &SourceFile,
    raw: &str,
    report: &mut ScanReport,
) -> anyhow::Result<()> {
    let (fm_ring, fm_lines) = frontmatter_ring(raw);
    let mut ring = fm_ring.unwrap_or(src.default_ring);
    // The native store's contract ring is 2: honor demotion to 5+ (skip),
    // never self-promotion into the resident/policy rings.
    if src.doc_path.starts_with("native:") {
        ring = ring.max(2);
    }
    if ring > MAX_INDEXED_RING {
        report.skipped_high_ring += 1;
        report.skipped.push(src.doc_path.clone());
        tx.execute(
            "INSERT INTO skipped_docs(path, mtime, size) VALUES(?1, ?2, ?3)
             ON CONFLICT(path) DO UPDATE SET mtime=excluded.mtime, size=excluded.size",
            rusqlite::params![src.doc_path, src.mtime as i64, src.size as i64],
        )?;
        return Ok(());
    }
    // Blank (not strip) private regions so line numbers stay accurate.
    let blanked = blank_private(raw);
    tx.execute(
        "INSERT INTO docs(path, ring, mtime, size) VALUES(?1, ?2, ?3, ?4)",
        rusqlite::params![src.doc_path, ring, src.mtime as i64, src.size as i64],
    )?;
    let doc_id = tx.last_insert_rowid();
    for target in wikilinks(&blanked) {
        tx.execute(
            "INSERT INTO doc_links(doc_id, target) VALUES(?1, ?2)",
            rusqlite::params![doc_id, target],
        )?;
    }
    // Citation context: every block carries its enclosing heading chain
    // (H1 > H2 > …); a heading block carries its ANCESTOR chain (a bare
    // heading has no other context); a table row additionally carries its
    // table's header row. `chain` is the dedup key part, `ctx` the searchable
    // and displayed form.
    let mut chain: Vec<(u8, String)> = Vec::new();
    let mut table_header: Option<String> = None;
    let mut last_row_end = 0usize;
    for (start, end, body) in segment(&blanked, fm_lines) {
        let chain_key;
        let ctx;
        if let Some((level, text)) = heading_of(&body) {
            chain.retain(|(l, _)| *l < level);
            chain_key = chain_text(&chain);
            ctx = chain_key.clone();
            chain.push((level, text));
            table_header = None;
        } else if body.trim_start().starts_with('|') {
            chain_key = chain_text(&chain);
            // A gap in line numbers separates two adjacent tables.
            if start > last_row_end + 1 {
                table_header = None;
            }
            last_row_end = end;
            match &table_header {
                Some(header) => {
                    ctx = if chain_key.is_empty() {
                        header.clone()
                    } else {
                        format!("{chain_key} > {header}")
                    };
                }
                None => {
                    // This row IS the table's header.
                    table_header = Some(snippet_of(&body));
                    ctx = chain_key.clone();
                }
            }
        } else {
            table_header = None;
            chain_key = chain_text(&chain);
            ctx = chain_key.clone();
        }
        let cite = cite_id(ring, &body);
        tx.prepare_cached(
            "INSERT INTO blocks(cite, doc_id, start_line, end_line, text, ctx, chain)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?
        .execute(rusqlite::params![cite, doc_id, start as i64, end as i64, body, ctx, chain_key])?;
        let block_id = tx.last_insert_rowid();
        tx.prepare_cached("INSERT INTO blocks_fts(rowid, text, ctx) VALUES(?1, ?2, ?3)")?
            .execute(rusqlite::params![block_id, body, ctx])?;
        report.blocks += 1;
    }
    report.docs += 1;
    Ok(())
}

/// Rebuilds the `links` table from `doc_links` + the doc registry. Every doc
/// is registered under ALL its path suffixes (stem, parent/stem, …, full
/// path; `.md` stripped, lowercased) and a target resolves only when its key
/// is unambiguous — a slash-qualified target resolves a stem collision
/// (brain-lint parity). A pure function of (docs, doc_links): full and
/// incremental scans converge on identical edges.
fn resolve_links(tx: &rusqlite::Transaction<'_>) -> anyhow::Result<()> {
    tx.execute("DELETE FROM links", [])?;
    let mut by_suffix: std::collections::HashMap<String, Option<i64>> =
        std::collections::HashMap::new();
    {
        let mut stmt = tx.prepare("SELECT id, path FROM docs")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        for (id, path) in rows.filter_map(Result::ok) {
            let stemless =
                path.strip_suffix(".md").unwrap_or(&path).to_ascii_lowercase();
            let parts: Vec<&str> = stemless.split('/').collect();
            for k in 1..=parts.len() {
                let key = parts[parts.len() - k..].join("/");
                by_suffix
                    .entry(key)
                    .and_modify(|e| *e = None) // ambiguous
                    .or_insert(Some(id));
            }
        }
    }
    let pending: Vec<(i64, String)> = {
        let mut stmt = tx.prepare("SELECT doc_id, target FROM doc_links")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        rows.filter_map(Result::ok).collect()
    };
    for (from_doc, target) in pending {
        let key = target.strip_suffix(".md").unwrap_or(&target);
        if let Some(Some(to_doc)) = by_suffix.get(key)
            && *to_doc != from_doc
        {
            tx.execute(
                "INSERT INTO links(from_doc, to_doc) VALUES(?1, ?2)",
                rusqlite::params![from_doc, to_doc],
            )?;
        }
    }
    Ok(())
}

/// Bumps the persisted catalog generation INSIDE the given transaction: a
/// reader can never observe a new catalog under an old generation or vice
/// versa.
fn bump_generation(tx: &rusqlite::Transaction<'_>) -> anyhow::Result<u64> {
    let generation: u64 = tx
        .query_row("SELECT value FROM meta WHERE key='generation'", [], |r| r.get::<_, String>(0))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
        + 1;
    tx.execute(
        "INSERT INTO meta(key, value) VALUES('generation', ?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [generation.to_string()],
    )?;
    Ok(generation)
}

fn set_fingerprint(tx: &rusqlite::Transaction<'_>, fingerprint: &str) -> anyhow::Result<()> {
    tx.execute(
        "INSERT INTO meta(key, value) VALUES('source_fingerprint', ?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [fingerprint],
    )?;
    Ok(())
}

/// Full rebuild inside one transaction. The corpus is small markdown; a
/// rebuild is cheap, and the incremental path ([`rescan_changed`]) exists for
/// the serving daemon's event batches.
pub fn scan(
    conn: &mut Connection,
    brain_root: &Path,
    native_root: Option<&Path>,
    rules: &RingRules,
) -> anyhow::Result<ScanReport> {
    let files = collect_files(brain_root, native_root, rules);
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
        "DELETE FROM vectors; DELETE FROM doc_links; DELETE FROM links;
         DELETE FROM blocks; DELETE FROM docs; DELETE FROM skipped_docs;
         INSERT INTO blocks_fts(blocks_fts) VALUES('delete-all');",
    )?;
    tx.execute(
        "INSERT INTO meta(key, value) VALUES('brain_root', ?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [brain_root.to_string_lossy().as_ref()],
    )?;
    set_fingerprint(&tx, &fingerprint)?;
    let generation = bump_generation(&tx)?;
    let mut report =
        ScanReport { docs: 0, blocks: 0, skipped_high_ring: 0, skipped: Vec::new(), generation };
    for (src, raw) in bodies {
        insert_doc(&tx, &src, &raw, &mut report)?;
    }
    resolve_links(&tx)?;
    tx.commit()?;
    Ok(report)
}

/// Removes one doc's rows everywhere: FTS rows (external-content FTS5 must be
/// told each removed row's old values), blocks (vectors cascade), the doc row
/// (links and doc_links cascade), and any `skipped_docs` entry.
fn delete_doc(tx: &rusqlite::Transaction<'_>, path: &str) -> anyhow::Result<()> {
    tx.execute("DELETE FROM skipped_docs WHERE path=?1", [path])?;
    let doc_id: Option<i64> =
        tx.query_row("SELECT id FROM docs WHERE path=?1", [path], |r| r.get(0)).ok();
    let Some(doc_id) = doc_id else { return Ok(()) };
    {
        let mut stmt = tx.prepare("SELECT id, text, ctx FROM blocks WHERE doc_id=?1")?;
        let rows = stmt.query_map([doc_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?;
        let blocks: Vec<(i64, String, String)> = rows.filter_map(Result::ok).collect();
        for (id, text, ctx) in blocks {
            tx.execute(
                "INSERT INTO blocks_fts(blocks_fts, rowid, text, ctx) VALUES('delete', ?1, ?2, ?3)",
                rusqlite::params![id, text, ctx],
            )?;
        }
    }
    tx.execute("DELETE FROM blocks WHERE doc_id=?1", [doc_id])?;
    tx.execute("DELETE FROM docs WHERE id=?1", [doc_id])?;
    Ok(())
}

/// Changed+vanished files an incremental rescan will handle; larger diffs
/// fall back to the full scan, whose bulk delete-all path is faster there.
const INCREMENTAL_MAX_CHANGES: usize = 32;

/// Incremental catalog update: re-reads ONLY the files whose (mtime, size)
/// changed — each changed doc's rows are deleted and reinserted, vanished
/// docs are pruned, links re-resolved — inside one transaction that also
/// advances the generation, exactly like [`scan`]. Returns `None` when the
/// incremental step has no valid basis (never scanned, different brain root)
/// or the diff exceeds [`INCREMENTAL_MAX_CHANGES`]; the caller then runs the
/// full scan. A no-op diff (fingerprint already current) commits nothing and
/// reports the existing generation.
pub fn rescan_changed(
    conn: &mut Connection,
    brain_root: &Path,
    native_root: Option<&Path>,
    rules: &RingRules,
) -> anyhow::Result<Option<ScanReport>> {
    let root_meta: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key='brain_root'", [], |r| r.get(0))
        .ok();
    if root_meta.as_deref() != Some(brain_root.to_string_lossy().as_ref())
        || generation(conn) == 0
    {
        return Ok(None);
    }
    let files = collect_files(brain_root, native_root, rules);
    let fingerprint = source_fingerprint(&files);
    let stored: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key='source_fingerprint'", [], |r| r.get(0))
        .ok();
    if stored.as_deref() == Some(fingerprint.as_str()) {
        // The committed catalog already describes this tree.
        return Ok(Some(ScanReport {
            docs: 0,
            blocks: 0,
            skipped_high_ring: 0,
            skipped: Vec::new(),
            generation: generation(conn),
        }));
    }
    // Stat diff against the stored per-file stats (indexed docs + skipped
    // ring-5+ files) — the same (mtime, size) basis the fingerprint uses.
    let mut stored_stats: std::collections::HashMap<String, (i64, i64)> =
        std::collections::HashMap::new();
    for table in ["docs", "skipped_docs"] {
        let mut stmt = conn.prepare(&format!("SELECT path, mtime, size FROM {table}"))?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))
        })?;
        for (path, stat) in rows.filter_map(Result::ok) {
            stored_stats.insert(path, stat);
        }
    }
    let changed: Vec<&SourceFile> = files
        .iter()
        .filter(|f| stored_stats.get(&f.doc_path) != Some(&(f.mtime as i64, f.size as i64)))
        .collect();
    let current: std::collections::HashSet<&str> =
        files.iter().map(|f| f.doc_path.as_str()).collect();
    let vanished: Vec<String> = stored_stats
        .keys()
        .filter(|p| !current.contains(p.as_str()))
        .cloned()
        .collect();
    if changed.len() + vanished.len() > INCREMENTAL_MAX_CHANGES {
        return Ok(None);
    }
    // Changed bodies read BEFORE the write transaction (same NFS rationale
    // as scan). A file that vanished between stat and read is treated as
    // deleted; the fingerprint diff surfaces it again on the next pass.
    let bodies: Vec<(&SourceFile, Option<String>)> = changed
        .iter()
        .map(|f| (*f, std::fs::read_to_string(&f.abs).ok()))
        .collect();
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for path in &vanished {
        delete_doc(&tx, path)?;
    }
    for (src, _) in &bodies {
        delete_doc(&tx, &src.doc_path)?;
    }
    set_fingerprint(&tx, &fingerprint)?;
    let generation = bump_generation(&tx)?;
    let mut report =
        ScanReport { docs: 0, blocks: 0, skipped_high_ring: 0, skipped: Vec::new(), generation };
    for (src, raw) in &bodies {
        if let Some(raw) = raw {
            insert_doc(&tx, src, raw, &mut report)?;
        }
    }
    resolve_links(&tx)?;
    tx.commit()?;
    Ok(Some(report))
}

/// Current catalog generation: the count of committed scans of this index.
/// Monotonic and persisted in meta; incremented INSIDE each scan's
/// transaction, so any reader always sees a (catalog, generation) pair from
/// one commit. 0 = never scanned.
pub fn generation(conn: &Connection) -> u64 {
    conn.query_row("SELECT value FROM meta WHERE key='generation'", [], |r| r.get::<_, String>(0))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Deterministic digest of the catalog: sha256 over the sorted
/// (cite, path, ring) rows. Two catalogs built from the same tree — by ANY
/// derivation path (fresh scan, event-driven rebuild, post-crash backstop) —
/// must produce the same checksum: the coherence invariant's cross-holder
/// verification value. Generation is deliberately NOT part of the digest.
pub fn catalog_checksum(conn: &Connection) -> anyhow::Result<String> {
    let mut stmt = conn.prepare(
        "SELECT b.cite, d.path, d.ring FROM blocks b JOIN docs d ON d.id = b.doc_id
         ORDER BY b.cite, d.path, d.ring",
    )?;
    let rows =
        stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?;
    let mut hasher = sha2::Sha256::new();
    for (cite, path, ring) in rows.filter_map(Result::ok) {
        hasher.update(cite.as_bytes());
        hasher.update([0u8]);
        hasher.update(path.as_bytes());
        hasher.update([0u8]);
        hasher.update(ring.to_le_bytes());
        hasher.update([0xffu8]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Read-only open for serving-side query threads: never contends for the
/// write lock, never creates or migrates schema. Fails when no index exists
/// yet — the caller reports that instead of racing the builder.
pub fn open_ro(state_dir: &Path) -> anyhow::Result<Connection> {
    use rusqlite::OpenFlags;
    let conn = Connection::open_with_flags(
        db_path(state_dir),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_millis(1000))?;
    Ok(conn)
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

/// Snippet with the citation context prepended: `[H1 > H2] text` (and, for
/// table rows, the table's header row inside the context). A bare heading or
/// a lone table row is unreadable without it.
fn snippet_with_ctx(ctx: &str, text: &str) -> String {
    let s = snippet_of(text);
    if ctx.is_empty() { s } else { format!("[{ctx}] {s}") }
}

/// The content-hash part of a citation id (after the `r<ring>-` prefix): the
/// key every copy of one logical block shares across stores and rings.
fn cite_hash(cite: &str) -> &str {
    cite.split_once('-').map_or(cite, |(_, hash)| hash)
}

/// Collapses hits carrying the same content hash UNDER THE SAME heading
/// chain: the native auto-memory store mirrors brain files, so one logical
/// block would otherwise surface once per store. Identical short blocks in
/// DIFFERENT sections (a bare "yes", a repeated table row) are different
/// statements and are NOT collapsed. The lowest-ring copy survives, at the
/// first occurrence's rank; the suppressed copies' paths land on the keeper's
/// `mirrors`.
fn dedup_by_content(hits: Vec<Hit>) -> Vec<Hit> {
    let mut out: Vec<Hit> = Vec::new();
    let mut slot_by_key: std::collections::HashMap<(String, String), usize> =
        std::collections::HashMap::new();
    for hit in hits {
        let key = (cite_hash(&hit.cite).to_string(), hit.chain.clone());
        match slot_by_key.get(&key) {
            Some(&slot) => {
                let kept = &mut out[slot];
                if hit.ring < kept.ring {
                    let old = std::mem::replace(kept, hit);
                    kept.mirrors = old.mirrors;
                    kept.mirrors.push(old.path);
                } else if hit.path != kept.path && !kept.mirrors.contains(&hit.path) {
                    kept.mirrors.push(hit.path);
                }
            }
            None => {
                slot_by_key.insert(key, out.len());
                out.push(hit);
            }
        }
    }
    out
}

/// Ring folded into the BM25 order as a SMALL additive prior (bm25 is
/// negative-better): enough to break near-ties toward the more trusted ring,
/// far below the gaps between genuinely different lexical matches.
const RING_PRIOR: f64 = 0.05;
/// Weight of the heading-chain context column in BM25: context terms help
/// find a block but must never outrank the same terms in the block itself.
const CTX_WEIGHT: f64 = 0.3;

/// The ranked SELECT shared by [`recall`] and [`bm25_block_ids`].
fn ranked_match_sql(select: &str) -> String {
    format!(
        "SELECT {select}
         FROM blocks_fts f
         JOIN blocks b ON b.id = f.rowid
         JOIN docs d ON d.id = b.doc_id
         WHERE blocks_fts MATCH ?1
         ORDER BY bm25(blocks_fts, 1.0, {CTX_WEIGHT}) + {RING_PRIOR} * d.ring, d.ring ASC
         LIMIT ?2"
    )
}

pub fn recall(conn: &Connection, query: &str, limit: usize) -> anyhow::Result<Vec<Hit>> {
    let fts = fts_query(query);
    if fts.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(&ranked_match_sql(
        "b.cite, d.path, d.ring, b.start_line, b.end_line, b.text, b.ctx, b.chain",
    ))?;
    // Twice the candidate pool: duplicate suppression must refill freed
    // slots with the next-ranked hits, never shrink the result count.
    let rows = stmt.query_map(rusqlite::params![fts, (limit * 2) as i64], |r| {
        Ok(Hit {
            cite: r.get(0)?,
            path: r.get(1)?,
            ring: r.get::<_, i64>(2)? as u8,
            start_line: r.get::<_, i64>(3)? as usize,
            end_line: r.get::<_, i64>(4)? as usize,
            snippet: snippet_with_ctx(&r.get::<_, String>(6)?, &r.get::<_, String>(5)?),
            mirrors: Vec::new(),
            chain: r.get(7)?,
        })
    })?;
    let mut hits = dedup_by_content(rows.filter_map(Result::ok).collect());
    // Ring-band slot reservation: rings 0-1 are the top-trust band — when
    // any of them matches at all, the top slot carries the best of them.
    // Everything else stays in BM25(+prior) order.
    if let Some(pos) = hits.iter().position(|h| h.ring <= 1)
        && pos > 0
    {
        let reserved = hits.remove(pos);
        hits.insert(0, reserved);
    }
    hits.truncate(limit);
    Ok(hits)
}

/// Expands a citation id to its full block(s) — the second disclosure layer.
/// Content-addressing means the HASH names the logical block while the ring
/// prefix only labels one copy's trust level: expansion matches every copy
/// sharing the hash (a mirror suppressed in recall stays reachable through
/// the kept hit's cite), lowest ring first.
pub fn expand(conn: &Connection, cite: &str) -> anyhow::Result<Vec<Block>> {
    let mut stmt = conn.prepare(
        "SELECT b.cite, d.path, d.ring, b.start_line, b.end_line, b.text
         FROM blocks b JOIN docs d ON d.id = b.doc_id
         WHERE substr(b.cite, instr(b.cite, '-') + 1) = ?1
         ORDER BY d.ring ASC, d.path ASC, b.start_line ASC",
    )?;
    let rows = stmt.query_map([cite_hash(cite)], |r| {
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
        "SELECT b.cite, d.path, d.ring, b.start_line, b.end_line, b.text, b.ctx, b.chain
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
                snippet: snippet_with_ctx(&r.get::<_, String>(6)?, &r.get::<_, String>(5)?),
                mirrors: Vec::new(),
                chain: r.get(7)?,
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

/// BM25-ranked block ids for the same query shape and order [`recall`] uses.
fn bm25_block_ids(conn: &Connection, query: &str, limit: usize) -> anyhow::Result<Vec<i64>> {
    let fts = fts_query(query);
    if fts.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(&ranked_match_sql("b.id"))?;
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
    rules: &RingRules,
) -> anyhow::Result<Connection> {
    let mut conn = open(state_dir).context("open index")?;
    if stale(&conn, brain_root, native_root, rules)? {
        // `None` = another rebuilder is active; serve the committed snapshot.
        let lock = crate::lockfile::acquire(&state_dir.join("scan.lock"), 500, 120);
        // Re-check under the lock: the previous holder may have rebuilt
        // exactly what we were about to.
        if lock.is_some() && stale(&conn, brain_root, native_root, rules)? {
            scan(&mut conn, brain_root, native_root, rules)?;
        }
    }
    Ok(conn)
}

#[cfg(test)]
mod path_shape_tests {
    use super::*;

    #[test]
    fn doc_paths_are_slash_separated_whatever_the_platform_uses() {
        assert_eq!(normalize_separators(r"mind\secrets\age.key", '\\'), "mind/secrets/age.key");
        assert_eq!(normalize_separators("mind/secrets/age.key", '/'), "mind/secrets/age.key");
        // A backslash is a legal filename character on unix and must survive.
        assert_eq!(normalize_separators(r"odd\name.md", '/'), r"odd\name.md");
    }

    #[test]
    fn the_exclusion_boundary_holds_for_backslash_separated_paths() {
        // Without normalization every one of these slips past the prefix
        // match and lands in the catalog — secrets first.
        for raw in [r"mind\secrets\age.key.md", r"logs\session.md", r"projects\repo\a.md", r"knowledge\archive\old.md", r".git\COMMIT_EDITMSG.md"] {
            let rel = normalize_separators(raw, '\\');
            assert!(excluded(&rel, &RingRules::default()), "{raw} normalized to {rel} must be excluded");
        }
        for raw in [r"mind\secrets", r"logs", r"projects", r"knowledge\archive"] {
            assert!(excluded_dir(&normalize_separators(raw, '\\'), &RingRules::default()), "{raw}");
        }
    }

    #[test]
    fn ring_defaults_survive_the_separator() {
        let rules = RingRules::default();
        assert_eq!(default_ring(&normalize_separators(r"mind\memories\MEMORY.md", '\\'), &rules), 1);
        assert_eq!(default_ring(&normalize_separators(r"mind\memories\topic.md", '\\'), &rules), 2);
        assert_eq!(default_ring(&normalize_separators(r"todo\active\x\STATUS.md", '\\'), &rules), 4);
    }

    #[test]
    fn secret_shaped_names_are_caught_after_normalization() {
        assert!(secret_shaped(&normalize_separators(r"knowledge\hosts\my.pem", '\\')));
        assert!(secret_shaped(&normalize_separators(r"knowledge\hosts\password-notes.md", '\\')));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RingRule;

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
        // Regression lock: with the shipped rules the taxonomy entry point
        // answers exactly what the hardcoded version answered.
        let r = RingRules::default();
        assert_eq!(default_ring("AGENT.md", &r), 1);
        assert_eq!(default_ring("README.md", &r), 1);
        assert_eq!(default_ring("mind/memories/MEMORY.md", &r), 1);
        assert_eq!(default_ring("mind/memories/feedback_x.md", &r), 2);
        assert_eq!(default_ring("knowledge/hosts/example/storage.md", &r), 3);
        assert_eq!(default_ring("todo/active/task/STATUS.md", &r), 4);
    }

    #[test]
    fn custom_ring_rules_drive_the_indexed_rings() {
        // A tree with none of the shipped names: rings come from config only.
        let dir = brain(&[
            ("laws.md", "kwenty is the hard law\n"),
            ("handbook/style.md", "trakkel is the house style\n"),
            ("misc/loose.md", "vurpel is a loose fact\n"),
            ("scratch/dump.md", "pargast is scratch\n"),
        ]);
        let rules = RingRules {
            rules: vec![
                RingRule { prefix: "laws.md".into(), ring: 0 },
                RingRule { prefix: "handbook/".into(), ring: 1 },
                RingRule { prefix: "scratch/".into(), ring: 5 },
                RingRule { prefix: String::new(), ring: 4 },
            ],
            exclude_prefixes: Vec::new(),
        };
        let mut conn = open(dir.path()).unwrap();
        let report = scan(&mut conn, dir.path(), None, &rules).unwrap();
        assert_eq!(report.skipped, vec!["scratch/dump.md".to_string()], "ring 5 is never indexed");
        let ring_of = |q: &str| recall(&conn, q, 5).unwrap().first().map(|h| h.ring);
        assert_eq!(ring_of("kwenty"), Some(0));
        assert_eq!(ring_of("trakkel"), Some(1));
        assert_eq!(ring_of("vurpel"), Some(4), "the catch-all rule applies");
        assert_eq!(ring_of("pargast"), None);
    }

    #[test]
    fn configured_exclusions_apply_while_hard_ones_cannot_be_lifted() {
        let dir = brain(&[
            ("mind/secrets/tokens.md", "hyllvar token\n"),
            ("drafts/wip.md", "nogrant draft\n"),
            ("knowledge/live.md", "kavender fact\n"),
        ]);
        let rules = RingRules {
            rules: RingRules::default().rules,
            exclude_prefixes: vec!["drafts/".into()],
        };
        let mut conn = open(dir.path()).unwrap();
        scan(&mut conn, dir.path(), None, &rules).unwrap();
        assert!(recall(&conn, "hyllvar", 5).unwrap().is_empty(), "secrets stay out, always");
        assert!(recall(&conn, "nogrant", 5).unwrap().is_empty(), "configured exclusion applies");
        assert_eq!(recall(&conn, "kavender", 5).unwrap().len(), 1);
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
        let report = scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
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
        scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
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
        let report = scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
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
        let report = scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
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
        let report = scan(&mut conn, brain.path(), Some(native.path()), &RingRules::default()).unwrap();
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
        scan(&mut conn, brain.path(), Some(native.path()), &RingRules::default()).unwrap();
        assert!(!stale(&conn, brain.path(), Some(native.path()), &RingRules::default()).unwrap());
        std::fs::write(mem.join("MEMORY.md"), "second, and quite a bit longer\n").unwrap();
        assert!(stale(&conn, brain.path(), Some(native.path()), &RingRules::default()).unwrap());
    }

    #[test]
    fn missing_native_root_is_fine() {
        let brain = brain(&[("knowledge/a.md", "alpha\n")]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        let absent = brain.path().join("no-such-dir");
        let report = scan(&mut conn, brain.path(), Some(&absent), &RingRules::default()).unwrap();
        assert_eq!(report.docs, 1);
        assert!(!stale(&conn, brain.path(), Some(&absent), &RingRules::default()).unwrap());
    }

    #[test]
    fn malformed_ring_frontmatter_fails_closed() {
        let dir = brain(&[
            ("knowledge/bad.md", "---\nring: banana\n---\nzweptahl must stay hidden\n"),
            ("knowledge/spaced.md", "---\nRing: 1 # promoted\n---\nquorvex is promoted\n"),
        ]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        let report = scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
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
        let report = scan(&mut conn, brain_dir.path(), Some(native.path()), &RingRules::default()).unwrap();
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
        scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
        assert!(!stale(&conn, dir.path(), None, &RingRules::default()).unwrap());
        // Editing the SKIPPED file (e.g. removing its ring-5 marker = promotion)
        // must be noticed — the old subset comparison was blind to this.
        std::fs::write(dir.path().join("knowledge/staged.md"), "now public\n").unwrap();
        assert!(stale(&conn, dir.path(), None, &RingRules::default()).unwrap());
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
    fn generation_advances_with_every_scan_and_persists() {
        let dir = brain(&[("knowledge/a.md", "alpha\n")]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        assert_eq!(generation(&conn), 0, "unscanned index has generation 0");
        let r1 = scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
        assert_eq!(r1.generation, 1);
        assert_eq!(generation(&conn), 1);
        let r2 = scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
        assert_eq!(r2.generation, 2, "generation is monotonic per committed scan");
        drop(conn);
        let conn = open(state.path()).unwrap();
        assert_eq!(generation(&conn), 2, "generation is persisted in index meta");
    }

    #[test]
    fn catalog_checksum_is_a_pure_function_of_the_tree() {
        let dir = brain(&[
            ("knowledge/a.md", "alpha fact\n\nbeta fact\n"),
            ("knowledge/b.md", "- gamma\n- delta\n"),
        ]);
        // Two independent derivations (separate state dirs) over the same tree.
        let s1 = tempfile::tempdir().unwrap();
        let s2 = tempfile::tempdir().unwrap();
        let mut c1 = open(s1.path()).unwrap();
        let mut c2 = open(s2.path()).unwrap();
        scan(&mut c1, dir.path(), None, &RingRules::default()).unwrap();
        scan(&mut c2, dir.path(), None, &RingRules::default()).unwrap();
        // Different generations must not leak into the digest.
        scan(&mut c2, dir.path(), None, &RingRules::default()).unwrap();
        let k1 = catalog_checksum(&c1).unwrap();
        let k2 = catalog_checksum(&c2).unwrap();
        assert!(!k1.is_empty());
        assert_eq!(k1, k2, "same tree must yield the same catalog checksum");
        // A content change must change the digest.
        std::fs::write(dir.path().join("knowledge/b.md"), "- gamma\n- delta prime\n").unwrap();
        scan(&mut c1, dir.path(), None, &RingRules::default()).unwrap();
        assert_ne!(catalog_checksum(&c1).unwrap(), k2);
    }

    #[test]
    fn open_ro_serves_the_committed_snapshot() {
        let dir = brain(&[("knowledge/a.md", "royw fact\n")]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
        let ro = open_ro(state.path()).unwrap();
        assert_eq!(recall(&ro, "royw", 5).unwrap().len(), 1);
        assert!(
            ro.execute("INSERT INTO meta(key,value) VALUES('x','y')", []).is_err(),
            "read-only handle must not be able to write"
        );
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
        scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
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
        scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
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
        scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
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
        scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
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
        scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
        for (id, _) in blocks_without_vectors(&conn, 10).unwrap() {
            insert_vector(&conn, id, &[1.0, 0.0]).unwrap();
        }
        assert_eq!(vector_counts(&conn).unwrap().0, 2);
        scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
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
        scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
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
        scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
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
    fn recall_suppresses_native_mirror_duplicates() {
        // The native auto-memory store mirrors brain files: the same logical
        // block must surface ONCE, as its lowest-ring copy, with the
        // suppressed paths recorded as mirrors.
        let brain = brain(&[(
            "mind/memories/MEMORY.md",
            "- zvol on btrfs needs the nossd mount option\n",
        )]);
        let native = tempfile::tempdir().unwrap();
        let mem = native.path().join("p/memory");
        std::fs::create_dir_all(&mem).unwrap();
        std::fs::write(mem.join("MEMORY.md"), "- zvol on btrfs needs the nossd mount option\n")
            .unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, brain.path(), Some(native.path()), &RingRules::default()).unwrap();
        let hits = recall(&conn, "nossd", 5).unwrap();
        assert_eq!(hits.len(), 1, "one logical block, one hit");
        assert_eq!(hits[0].ring, 1, "the lowest-ring copy survives");
        assert_eq!(hits[0].path, "mind/memories/MEMORY.md");
        assert_eq!(hits[0].mirrors, vec!["native:p/MEMORY.md".to_string()]);
    }

    #[test]
    fn dedup_keeps_lowest_ring_regardless_of_rank_order() {
        let h = |cite: &str, path: &str, ring: u8, chain: &str| Hit {
            cite: cite.into(),
            path: path.into(),
            ring,
            start_line: 1,
            end_line: 1,
            snippet: String::new(),
            mirrors: Vec::new(),
            chain: chain.into(),
        };
        let hits = vec![
            h("r2-aabbccddee", "native:p/MEMORY.md", 2, "Memory"),
            h("r1-aabbccddee", "mind/memories/MEMORY.md", 1, "Memory"),
            h("r3-0123456789", "knowledge/x.md", 3, ""),
        ];
        let out = dedup_by_content(hits);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].cite, "r1-aabbccddee", "lowest ring wins even when ranked second");
        assert_eq!(out[0].mirrors, vec!["native:p/MEMORY.md".to_string()]);
        assert_eq!(out[1].cite, "r3-0123456789");
        assert!(out[1].mirrors.is_empty(), "a hit without duplicates is untouched");
    }

    #[test]
    fn identical_blocks_in_different_sections_are_not_mirrors() {
        let h = |cite: &str, path: &str, ring: u8, chain: &str| Hit {
            cite: cite.into(),
            path: path.into(),
            ring,
            start_line: 1,
            end_line: 1,
            snippet: String::new(),
            mirrors: Vec::new(),
            chain: chain.into(),
        };
        let hits = vec![
            h("r3-aabbccddee", "knowledge/a.md", 3, "Hosts > Server"),
            h("r3-aabbccddee", "knowledge/b.md", 3, "Backups"),
        ];
        let out = dedup_by_content(hits);
        assert_eq!(out.len(), 2, "same hash under different heading chains = two statements");
        assert!(out.iter().all(|h| h.mirrors.is_empty()));
    }

    #[test]
    fn dedup_refills_freed_limit_slots() {
        // The mirrored block is the shortest (= best BM25), so a naive
        // LIMIT 2 would return both copies of it and lose the second logical
        // block. Suppression must not shrink the result count.
        let brain = brain(&[
            ("mind/memories/MEMORY.md", "- flumox\n"),
            ("knowledge/other.md", "flumox beta fact with more words\n"),
        ]);
        let native = tempfile::tempdir().unwrap();
        let mem = native.path().join("p/memory");
        std::fs::create_dir_all(&mem).unwrap();
        std::fs::write(mem.join("MEMORY.md"), "- flumox\n").unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, brain.path(), Some(native.path()), &RingRules::default()).unwrap();
        let hits = recall(&conn, "flumox", 2).unwrap();
        assert_eq!(hits.len(), 2, "suppression must not shrink the result count");
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert!(paths.contains(&"mind/memories/MEMORY.md"));
        assert!(paths.contains(&"knowledge/other.md"));
        let kept = hits.iter().find(|h| h.path == "mind/memories/MEMORY.md").unwrap();
        assert_eq!(kept.mirrors, vec!["native:p/MEMORY.md".to_string()]);
    }

    #[test]
    fn expand_matches_hash_across_rings_lowest_first() {
        let dir = brain(&[
            ("knowledge/copy.md", "shared statement body\n"),
            ("knowledge/promoted.md", "---\nring: 1\n---\nshared statement body\n"),
        ]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
        let hits = recall(&conn, "shared statement", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].ring, 1);
        assert_eq!(hits[0].mirrors, vec!["knowledge/copy.md".to_string()]);
        let blocks = expand(&conn, &hits[0].cite).unwrap();
        assert_eq!(blocks.len(), 2, "every copy of the logical block surfaces");
        assert_eq!(blocks[0].ring, 1, "lowest ring first");
        assert_eq!(blocks[0].path, "knowledge/promoted.md");
        assert_eq!(blocks[1].ring, 3);
        assert_eq!(blocks[1].path, "knowledge/copy.md");
    }

    #[test]
    fn scan_reports_skipped_paths() {
        let dir = brain(&[
            ("knowledge/ok.md", "fine\n"),
            ("knowledge/staged.md", "---\nring: 5\n---\nquarantined\n"),
            ("knowledge/broken.md", "---\nring: banana\n---\nunparseable\n"),
        ]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        let report = scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
        assert_eq!(report.skipped_high_ring, 2);
        assert_eq!(
            report.skipped,
            vec!["knowledge/broken.md".to_string(), "knowledge/staged.md".to_string()],
            "skipped paths listed in doc-path order"
        );
    }

    #[test]
    fn tilde_fences_segment_like_backticks() {
        let text = "~~~\ncode line\nstill code\n~~~\n\nafter\n";
        let blocks = segment(text, 0);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].2, "~~~\ncode line\nstill code\n~~~");
        assert_eq!((blocks[0].0, blocks[0].1), (1, 4));
        assert_eq!(blocks[1].2, "after");
        // A tilde fence containing backtick runs must not close early, and
        // vice versa.
        let text = "~~~\n```\ninner\n```\n~~~\nafter\n";
        let blocks = segment(text, 0);
        assert_eq!(blocks.len(), 2, "backtick runs inside a tilde fence do not close it");
        assert_eq!((blocks[0].0, blocks[0].1), (1, 5));
        let text = "```\n~~~\ninner\n~~~\n```\n";
        let blocks = segment(text, 0);
        assert_eq!(blocks.len(), 1, "tilde runs inside a backtick fence do not close it");
        assert_eq!((blocks[0].0, blocks[0].1), (1, 5));
    }

    #[test]
    fn setext_headings_are_heading_blocks() {
        let text = "Title\n=====\n\nintro paragraph\n\nSection\n-------\n\nbody text\n";
        let blocks = segment(text, 0);
        let bodies: Vec<&str> = blocks.iter().map(|(_, _, b)| b.as_str()).collect();
        assert_eq!(bodies, vec!["Title\n=====", "intro paragraph", "Section\n-------", "body text"]);
        assert_eq!((blocks[0].0, blocks[0].1), (1, 2), "H1 spans text + underline");
        assert_eq!((blocks[2].0, blocks[2].1), (6, 7), "H2 spans text + underline");
        assert_eq!(heading_of(blocks[0].2.as_str()), Some((1, "Title".to_string())));
        assert_eq!(heading_of(blocks[2].2.as_str()), Some((2, "Section".to_string())));
        // A multi-line paragraph before an underline: only the LAST line is
        // underlined into the heading; earlier lines stay a paragraph.
        let text = "para line\nTitle\n====\n";
        let blocks = segment(text, 0);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].2, "para line");
        assert_eq!((blocks[0].0, blocks[0].1), (1, 1));
        assert_eq!(blocks[1].2, "Title\n====");
        assert_eq!((blocks[1].0, blocks[1].1), (2, 3));
    }

    #[test]
    fn numbered_items_keep_blank_separated_indented_continuation() {
        let text = "1. item one\n\n   continuation para of item one\n\n2. item two\n";
        let blocks = segment(text, 0);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].2, "1. item one\n\n   continuation para of item one");
        assert_eq!((blocks[0].0, blocks[0].1), (1, 3), "exact line span including the blank");
        assert_eq!(blocks[1].2, "2. item two");
        assert_eq!((blocks[1].0, blocks[1].1), (5, 5));
    }

    #[test]
    fn paren_numbered_items_split_like_dotted_ones() {
        let blocks = segment("1) item one\n2) item two\n", 0);
        assert_eq!(blocks.len(), 2, "CommonMark `1)` ordered markers are list items");
        assert_eq!(blocks[0].2, "1) item one");
        assert_eq!(blocks[1].2, "2) item two");
    }

    #[test]
    fn tab_indented_continuation_stays_with_the_item() {
        let blocks = segment("1. item one\n\tcontinued with a tab\n", 0);
        assert_eq!(blocks.len(), 1);
        assert_eq!((blocks[0].0, blocks[0].1), (1, 2));
    }

    #[test]
    fn heading_chain_prepended_to_snippets() {
        let dir = brain(&[(
            "knowledge/h.md",
            "# ProjectX\n\n## Setup\n\nInstall the flurbium package.\n",
        )]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
        let hits = recall(&conn, "flurbium", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "[ProjectX > Setup] Install the flurbium package.");
        // A bare heading block carries its ANCESTOR chain.
        let hits = recall(&conn, "setup", 5).unwrap();
        let heading = hits.iter().find(|h| h.snippet.contains("## Setup")).unwrap();
        assert_eq!(heading.snippet, "[ProjectX] ## Setup");
    }

    #[test]
    fn table_rows_carry_header_and_chain() {
        let dir = brain(&[(
            "knowledge/t.md",
            "# Hosts\n\n| Name | IP |\n|---|---|\n| serverx | 192.0.2.1 |\n",
        )]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
        let hits = recall(&conn, "serverx", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "[Hosts > | Name | IP |] | serverx | 192.0.2.1 |");
        // The header row itself gets only the heading chain.
        let hits = recall(&conn, "name", 5).unwrap();
        let header = hits.iter().find(|h| h.snippet.contains("| Name | IP |")).unwrap();
        assert_eq!(header.snippet, "[Hosts] | Name | IP |");
    }

    #[test]
    fn chain_matches_recall_but_rank_below_text_matches() {
        // Filler blocks keep the term's IDF positive (BM25 inverts its
        // ranking when a term appears in most blocks of a tiny corpus).
        let dir = brain(&[
            ("knowledge/a.md", "glorpnik\n"),
            ("knowledge/b.md", "# glorpnik\n\nunrelated words here\n"),
            ("knowledge/fill.md", "- one\n- two\n- three\n- four\n- five\n- six\n- seven\n- eight\n"),
        ]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
        let hits = recall(&conn, "glorpnik", 10).unwrap();
        let ctx_only = hits
            .iter()
            .position(|h| h.snippet.contains("unrelated words"))
            .expect("a block whose HEADING CHAIN matches must still be recalled");
        assert!(
            hits[0].snippet.contains("glorpnik"),
            "a text match leads: {:?}",
            hits.iter().map(|h| &h.snippet).collect::<Vec<_>>()
        );
        assert_eq!(
            ctx_only,
            hits.len() - 1,
            "the chain-only match ranks below every text match (lower column weight)"
        );
    }

    #[test]
    fn same_text_under_different_headings_is_two_hits_not_mirrors() {
        let dir = brain(&[
            ("knowledge/a.md", "# Server\n\n- restart the service\n"),
            ("knowledge/b.md", "# Laptop\n\n- restart the service\n"),
        ]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
        let hits = recall(&conn, "restart", 5).unwrap();
        assert_eq!(hits.len(), 2, "different sections = different statements");
        assert!(hits.iter().all(|h| h.mirrors.is_empty()));
    }

    #[test]
    fn wikilinks_inside_code_fences_are_ignored() {
        let text = "```\n[[nope]]\n```\nsee [[yes]]\n~~~\n[[also-nope]]\n~~~\n";
        assert_eq!(wikilinks(text), vec!["yes"]);
    }

    #[test]
    fn slash_qualified_wikilink_resolves_an_ambiguous_stem() {
        let dir = brain(&[
            ("knowledge/a/readme2.md", "x\n"),
            ("knowledge/b/readme2.md", "y\n"),
            ("knowledge/linker.md", "see [[a/readme2]] and [[knowledge/b/readme2]]\n"),
        ]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
        let linked = linked_docs(&conn, &["knowledge/linker.md".to_string()], 8).unwrap();
        let paths: Vec<&str> = linked.iter().map(|(p, _)| p.as_str()).collect();
        assert!(paths.contains(&"knowledge/a/readme2.md"), "parent/stem suffix resolves: {paths:?}");
        assert!(paths.contains(&"knowledge/b/readme2.md"), "full-path suffix resolves: {paths:?}");
    }

    #[test]
    fn ring0_1_match_takes_the_top_slot() {
        // The ring-3 block is the stronger lexical match (shortest); the
        // ring-1 statement must still hold the top slot.
        let dir = brain(&[
            ("AGENT.md", "- the fenwick rule with several more words around it\n"),
            ("knowledge/deep.md", "fenwick\n"),
            // Fillers keep the IDF positive so BM25 genuinely prefers the
            // shorter ring-3 block before the reservation kicks in.
            ("knowledge/fill.md", "- one\n- two\n- three\n- four\n- five\n- six\n- seven\n- eight\n"),
        ]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
        let hits = recall(&conn, "fenwick", 5).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].ring, 1, "top slot reserved for the ring 0-1 band");
        assert_eq!(hits[1].ring, 3, "the rest stays in BM25 order");
    }

    #[test]
    fn ring_prior_breaks_ties_but_never_strong_lexical_wins() {
        // Equal lexical evidence: the lower ring wins the near-tie.
        let dir = brain(&[
            ("knowledge/a.md", "zelkova pattern alpha\n"),
            ("todo/x/b.md", "zelkova pattern bravo\n"),
        ]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
        let hits = recall(&conn, "zelkova", 5).unwrap();
        assert_eq!(hits[0].ring, 3, "prior breaks the tie toward the lower ring");
        assert_eq!(hits[1].ring, 4);
        // Strong lexical win: the much better match stays first even from a
        // higher ring (no ring 0-1 hit involved, so no reservation either).
        let dir = brain(&[
            ("knowledge/a.md", "korvat is mentioned once amid many other words in this long statement about something else\n"),
            ("todo/x/b.md", "korvat\n"),
            ("knowledge/fill.md", "- one\n- two\n- three\n- four\n- five\n- six\n- seven\n- eight\n"),
        ]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
        let hits = recall(&conn, "korvat", 5).unwrap();
        assert_eq!(hits[0].ring, 4, "a strong lexical win is never overridden by the prior");
    }

    #[test]
    fn rescan_changed_matches_a_fresh_scan_exactly() {
        let dir = brain(&[
            ("knowledge/a.md", "# Hosts\n\n| Name | IP |\n| serverx | 192.0.2.1 |\n"),
            ("knowledge/b.md", "- keep this\n"),
            ("knowledge/c.md", "doomed content\n"),
        ]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        let r1 = scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
        assert_eq!(r1.generation, 1);
        // Vectors on every block: unchanged docs must keep theirs.
        for (id, _) in blocks_without_vectors(&conn, 100).unwrap() {
            insert_vector(&conn, id, &[1.0, 0.0]).unwrap();
        }
        let vectors_before = vector_counts(&conn).unwrap().0;

        // Change one file, add one (with a wikilink), delete one.
        std::fs::write(dir.path().join("knowledge/b.md"), "- keep this\n- brand new fact\n").unwrap();
        std::fs::write(dir.path().join("knowledge/d.md"), "arrived later, see [[a]]\n").unwrap();
        std::fs::remove_file(dir.path().join("knowledge/c.md")).unwrap();

        let r2 = rescan_changed(&mut conn, dir.path(), None, &RingRules::default()).unwrap().expect("small diff → incremental");
        assert_eq!(r2.generation, 2, "incremental commit advances the generation");
        assert!(!stale(&conn, dir.path(), None, &RingRules::default()).unwrap());

        // Byte-identical catalog vs an independent fresh derivation.
        let state2 = tempfile::tempdir().unwrap();
        let mut fresh = open(state2.path()).unwrap();
        scan(&mut fresh, dir.path(), None, &RingRules::default()).unwrap();
        assert_eq!(
            catalog_checksum(&conn).unwrap(),
            catalog_checksum(&fresh).unwrap(),
            "incremental and fresh derivations must agree byte-for-byte"
        );

        // Content moved correctly.
        assert_eq!(recall(&conn, "brand", 5).unwrap().len(), 1);
        assert_eq!(recall(&conn, "arrived", 5).unwrap().len(), 1);
        assert!(recall(&conn, "doomed", 5).unwrap().is_empty());
        // The changed doc's snippet still carries its rebuilt table context.
        let hits = recall(&conn, "serverx", 5).unwrap();
        assert_eq!(hits[0].snippet, "[Hosts > | Name | IP |] | serverx | 192.0.2.1 |");
        // Links of the NEW doc resolved (full re-resolution each rescan).
        let linked = linked_docs(&conn, &["knowledge/d.md".to_string()], 8).unwrap();
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].0, "knowledge/a.md");
        // Unchanged docs kept their vectors; changed/removed lost theirs.
        let (vectors_after, _) = vector_counts(&conn).unwrap();
        let a_blocks = 3; // heading + 2 table rows
        assert_eq!(vectors_after, a_blocks, "only the untouched doc's vectors survive");
        assert!(vectors_before > vectors_after);

        // A no-op rescan commits nothing and keeps the generation.
        let r3 = rescan_changed(&mut conn, dir.path(), None, &RingRules::default()).unwrap().unwrap();
        assert_eq!(r3.generation, 2, "fingerprint already current → no new commit");
        assert_eq!(generation(&conn), 2);
    }

    #[test]
    fn rescan_falls_back_without_basis_or_on_large_diffs() {
        let dir = brain(&[("knowledge/a.md", "alpha\n")]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        // Never scanned: no basis.
        assert!(rescan_changed(&mut conn, dir.path(), None, &RingRules::default()).unwrap().is_none());
        scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
        // Different brain root: no basis.
        let other = brain(&[("knowledge/a.md", "alpha\n")]);
        assert!(rescan_changed(&mut conn, other.path(), None, &RingRules::default()).unwrap().is_none());
        // A diff beyond the incremental cap falls back to the full scan.
        for i in 0..40 {
            std::fs::write(dir.path().join(format!("knowledge/bulk{i}.md")), "bulk\n").unwrap();
        }
        assert!(rescan_changed(&mut conn, dir.path(), None, &RingRules::default()).unwrap().is_none());
        assert!(stale(&conn, dir.path(), None, &RingRules::default()).unwrap(), "fallback leaves the full scan to do it");
    }

    #[test]
    fn rescan_handles_ring5_transitions_both_ways() {
        let dir = brain(&[
            ("knowledge/staged.md", "---\nring: 5\n---\nquarantined zulqar\n"),
            ("knowledge/normal.md", "public wembly fact\n"),
        ]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
        assert!(recall(&conn, "zulqar", 5).unwrap().is_empty());

        // Promotion: the ring-5 marker is removed.
        std::fs::write(dir.path().join("knowledge/staged.md"), "promoted zulqar\n").unwrap();
        // Demotion: a public file becomes staged.
        std::fs::write(dir.path().join("knowledge/normal.md"), "---\nring: 5\n---\npublic wembly fact\n").unwrap();
        let r = rescan_changed(&mut conn, dir.path(), None, &RingRules::default()).unwrap().expect("incremental");
        assert_eq!(r.skipped_high_ring, 1);
        assert_eq!(recall(&conn, "zulqar", 5).unwrap().len(), 1, "promoted file is indexed");
        assert!(recall(&conn, "wembly", 5).unwrap().is_empty(), "demoted file left the index");
        assert!(!stale(&conn, dir.path(), None, &RingRules::default()).unwrap(), "skipped-file stats are tracked too");
        // The tracked skipped file changing again is still an incremental step.
        std::fs::write(dir.path().join("knowledge/normal.md"), "---\nring: 5\n---\nstill hidden, edited\n").unwrap();
        let r = rescan_changed(&mut conn, dir.path(), None, &RingRules::default()).unwrap().expect("incremental");
        assert_eq!(r.skipped_high_ring, 1);
        assert!(!stale(&conn, dir.path(), None, &RingRules::default()).unwrap());
    }

    #[test]
    fn staleness_flips_on_edit_and_rescan_clears_it() {
        let dir = brain(&[("knowledge/a.md", "alpha\n")]);
        let state = tempfile::tempdir().unwrap();
        let mut conn = open(state.path()).unwrap();
        scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
        assert!(!stale(&conn, dir.path(), None, &RingRules::default()).unwrap());
        std::fs::write(dir.path().join("knowledge/a.md"), "alpha beta, much longer now\n").unwrap();
        assert!(stale(&conn, dir.path(), None, &RingRules::default()).unwrap());
        scan(&mut conn, dir.path(), None, &RingRules::default()).unwrap();
        assert!(!stale(&conn, dir.path(), None, &RingRules::default()).unwrap());
        assert_eq!(recall(&conn, "beta", 5).unwrap().len(), 1);
    }
}
