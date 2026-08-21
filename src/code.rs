//! The code index (Milestone 3): tree-sitter symbol extraction over the
//! configured code roots, stored in the same per-host SQLite cache, served by
//! `cfetch find` with exact line ranges — read one function instead of a file.
//!
//! Symbol boundaries come from the syntax tree, never from regex line
//! guessing (measured upstream: 97% of guessed end lines were wrong). Only
//! whole-file parses; a file that fails to parse contributes no symbols
//! rather than wrong ones.

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use tree_sitter::{Language, Node, Parser};

#[derive(Debug, PartialEq)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    pub start_line: usize, // 1-indexed, inclusive
    pub end_line: usize,   // 1-indexed, inclusive
}

pub struct FindHit {
    pub path: String,
    pub name: Option<String>,
    pub kind: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub score: i64,
    pub token_estimate: u64,
    /// Import-graph importance percentile of the containing file (None when
    /// the project has no resolvable import edges — no signal, no score).
    pub rank_pct: Option<f64>,
}

enum Lang {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Go,
}

fn lang_of(path: &Path) -> Option<Lang> {
    match path.extension()?.to_str()? {
        "rs" => Some(Lang::Rust),
        "ts" | "mts" | "cts" => Some(Lang::TypeScript),
        "tsx" => Some(Lang::Tsx),
        "js" | "mjs" | "cjs" | "jsx" => Some(Lang::JavaScript),
        "py" => Some(Lang::Python),
        "go" => Some(Lang::Go),
        _ => None,
    }
}

fn language(lang: &Lang) -> Language {
    match lang {
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
        Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Lang::Python => tree_sitter_python::LANGUAGE.into(),
        Lang::Go => tree_sitter_go::LANGUAGE.into(),
    }
}

/// Node kinds that define a named symbol, per language. Wrapper nodes
/// (export statements, decorated definitions) are descended through, so an
/// exported function is found inside its wrapper.
fn symbol_kinds(lang: &Lang) -> &'static [&'static str] {
    match lang {
        Lang::Rust => &[
            "function_item",
            "struct_item",
            "enum_item",
            "trait_item",
            "mod_item",
            "macro_definition",
        ],
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => &[
            "function_declaration",
            "generator_function_declaration",
            "class_declaration",
            "method_definition",
            "interface_declaration",
            "type_alias_declaration",
            "enum_declaration",
        ],
        Lang::Python => &["function_definition", "class_definition"],
        Lang::Go => &["function_declaration", "method_declaration", "type_spec"],
    }
}

fn node_name(node: &Node, src: &[u8]) -> Option<String> {
    let name_node = node.child_by_field_name("name")?;
    Some(name_node.utf8_text(src).ok()?.to_string())
}

/// Extracts named symbols from source text. Returns None when the language is
/// unsupported or the parser fails — "not measured" is distinct from
/// "measured, found nothing".
pub fn extract(path: &Path, source: &str) -> Option<Vec<Symbol>> {
    let lang = lang_of(path)?;
    let mut parser = Parser::new();
    parser.set_language(&language(&lang)).ok()?;
    let tree = parser.parse(source, None)?;
    let kinds = symbol_kinds(&lang);
    let src = source.as_bytes();
    let mut out = Vec::new();
    walk(tree.root_node(), kinds, src, &mut out);
    Some(out)
}

fn walk(node: Node, kinds: &[&str], src: &[u8], out: &mut Vec<Symbol>) {
    if kinds.contains(&node.kind())
        && let Some(name) = node_name(&node, src)
    {
        out.push(Symbol {
            name,
            kind: node.kind().to_string(),
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
        });
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, kinds, src, out);
    }
}

/// Directories never worth indexing inside a code root.
fn skip_dir(name: &str) -> bool {
    matches!(
        name,
        "target" | "node_modules" | "dist" | "build" | ".git" | "vendor" | "__pycache__" | ".venv"
    )
}

pub struct CodeScanReport {
    pub files: usize,
    pub symbols: usize,
    /// Total import edges persisted after the scan (not just this pass's).
    pub edges: usize,
}

/// Whether the code index would take this file at all (used by the import
/// graph to refuse edges onto non-indexable targets).
pub(crate) fn is_indexable(path: &Path) -> bool {
    lang_of(path).is_some()
}

pub fn ensure_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS code_files(
           id INTEGER PRIMARY KEY,
           path TEXT UNIQUE NOT NULL,
           mtime INTEGER NOT NULL,
           size INTEGER NOT NULL,
           rank_pct REAL,
           norm_stem TEXT NOT NULL DEFAULT '',
           norm_path TEXT NOT NULL DEFAULT ''
         );
         CREATE TABLE IF NOT EXISTS symbols(
           id INTEGER PRIMARY KEY,
           file_id INTEGER NOT NULL REFERENCES code_files(id) ON DELETE CASCADE,
           name TEXT NOT NULL,
           kind TEXT NOT NULL,
           norm TEXT NOT NULL DEFAULT '',
           start_line INTEGER NOT NULL,
           end_line INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS symbols_norm ON symbols(norm);
         CREATE INDEX IF NOT EXISTS code_files_norm_stem ON code_files(norm_stem);",
    )?;
    crate::graph::ensure_schema(conn)?;
    Ok(())
}

/// Row count of the code index — lets callers distinguish "no hits" from
/// "never scanned".
pub fn file_count(conn: &Connection) -> anyhow::Result<i64> {
    ensure_schema(conn)?;
    Ok(conn.query_row("SELECT count(*) FROM code_files", [], |r| r.get(0))?)
}

/// One parallel walker's verdict on one file, sent to the writer thread.
enum WalkMsg {
    /// (mtime, size) unchanged: rows kept, no parse.
    Unchanged(String),
    /// Parsed (or unparseable — empty symbols) content to (re)insert.
    Parsed {
        path: String,
        mtime: i64,
        size: i64,
        symbols: Vec<Symbol>,
        edges: Vec<PathBuf>,
    },
}

/// The normalized file-name stem and full path of a code file — the
/// SQL-side match keys `find` filters on.
fn norm_keys(path: &str) -> (String, String) {
    // The argument is an ABSOLUTE operating-system path, so the basename must
    // be taken with the platform's separator rules: `rsplit('/')` hands back
    // the whole of `C:\repo\src\main.rs`, which would make every file-name
    // key on Windows a full path and `find <name>` match nothing.
    let base = Path::new(path).file_name().and_then(|s| s.to_str()).unwrap_or(path);
    let stem = base.rsplit_once('.').map(|(s, _)| s).unwrap_or(base);
    (norm_ident(stem), norm_ident(path))
}

/// Incremental per-file sync: unchanged (mtime, size) files keep their rows —
/// code corpora are much larger than the markdown brain, so the full-rebuild
/// shortcut does not carry over.
///
/// Parallel by construction: the ignore crate's parallel walker stats, reads
/// and tree-sitter-parses on one worker per core, feeding a channel; this
/// thread drains it into the ONE write transaction (SQLite has a single
/// writer anyway — parsing, not inserting, is the expensive part).
pub fn scan_code(conn: &mut Connection, roots: &[PathBuf]) -> anyhow::Result<CodeScanReport> {
    ensure_schema(conn)?;
    let mut report = CodeScanReport { files: 0, symbols: 0, edges: 0 };
    // Known (mtime, size) per path, read once — the walker threads make the
    // skip decision without touching the DB.
    let known: std::collections::HashMap<String, (i64, i64)> = {
        let mut stmt = conn.prepare("SELECT path, mtime, size FROM code_files")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))
        })?;
        rows.filter_map(Result::ok).collect()
    };
    let tx = conn.transaction()?;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for root in roots {
        let (msg_tx, msg_rx) = std::sync::mpsc::channel::<WalkMsg>();
        let known = &known;
        std::thread::scope(|s| -> anyhow::Result<()> {
            s.spawn(move || {
                ignore::WalkBuilder::new(root)
                    .hidden(true)
                    .git_ignore(true)
                    .follow_links(false)
                    .filter_entry(|e| {
                        e.file_name().to_str().map(|n| !skip_dir(n)).unwrap_or(true)
                    })
                    .build_parallel()
                    .run(|| {
                        let msg_tx = msg_tx.clone();
                        Box::new(move |entry| {
                            let Ok(entry) = entry else { return ignore::WalkState::Continue };
                            let Ok(meta) = entry.metadata() else {
                                return ignore::WalkState::Continue;
                            };
                            if !meta.is_file() || lang_of(entry.path()).is_none() {
                                return ignore::WalkState::Continue;
                            }
                            let path = entry.path().to_string_lossy().to_string();
                            let mtime = meta
                                .modified()
                                .ok()
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_secs())
                                .unwrap_or(0) as i64;
                            let size = meta.len() as i64;
                            if known.get(&path) == Some(&(mtime, size)) {
                                let _ = msg_tx.send(WalkMsg::Unchanged(path));
                                return ignore::WalkState::Continue;
                            }
                            let Ok(source) = std::fs::read_to_string(entry.path()) else {
                                return ignore::WalkState::Continue;
                            };
                            let symbols = extract(entry.path(), &source).unwrap_or_default();
                            // Import edges refresh with the file that declares
                            // them; targets added later self-heal on the
                            // importer's next change.
                            let edges = crate::graph::extract_edges(entry.path(), &source, root);
                            let _ = msg_tx
                                .send(WalkMsg::Parsed { path, mtime, size, symbols, edges });
                            ignore::WalkState::Continue
                        })
                    });
                // The factory's original sender drops here; the drain loop
                // below ends when the last walker clone is gone.
            });
            for msg in msg_rx {
                match msg {
                    WalkMsg::Unchanged(path) => {
                        seen.insert(path);
                        report.files += 1;
                    }
                    WalkMsg::Parsed { path, mtime, size, symbols, edges } => {
                        crate::graph::replace_file_edges(&tx, &path, &edges)?;
                        // prepare_cached: these two run once per changed file
                        // and once per symbol — re-preparing them would make
                        // the single writer the bottleneck under the parallel
                        // parse workers.
                        tx.prepare_cached("DELETE FROM code_files WHERE path=?1")?
                            .execute([&path])?;
                        let (norm_stem, norm_path) = norm_keys(&path);
                        tx.prepare_cached(
                            "INSERT INTO code_files(path, mtime, size, norm_stem, norm_path)
                             VALUES(?1, ?2, ?3, ?4, ?5)",
                        )?
                        .execute(rusqlite::params![path, mtime, size, norm_stem, norm_path])?;
                        let file_id = tx.last_insert_rowid();
                        let mut ins = tx.prepare_cached(
                            "INSERT INTO symbols(file_id, name, kind, norm, start_line, end_line)
                             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
                        )?;
                        for s in &symbols {
                            ins.execute(rusqlite::params![
                                file_id,
                                s.name,
                                s.kind,
                                norm_ident(&s.name),
                                s.start_line as i64,
                                s.end_line as i64
                            ])?;
                        }
                        drop(ins);
                        seen.insert(path);
                        report.files += 1;
                        report.symbols += symbols.len();
                    }
                }
            }
            Ok(())
        })?;
    }
    // Files gone from disk leave the index (ON DELETE CASCADE covers symbols).
    {
        let mut del = tx.prepare("SELECT id, path FROM code_files")?;
        let stale_ids: Vec<i64> = del
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .filter_map(Result::ok)
            .filter(|(_, p)| !seen.contains(p))
            .map(|(id, _)| id)
            .collect();
        drop(del);
        for id in stale_ids {
            tx.execute("DELETE FROM code_files WHERE id=?1", [id])?;
        }
    }
    // Importance follows the edges in the same transaction: a scan must
    // never commit files whose stored percentiles describe an older graph.
    crate::graph::prune_edges(&tx)?;
    crate::graph::recompute_ranks(&tx, roots)?;
    report.edges =
        tx.query_row("SELECT count(*) FROM import_edges", [], |r| r.get::<_, i64>(0))? as usize;
    tx.commit()?;
    Ok(report)
}

/// Case- and separator-insensitive identifier normalization so that
/// `resolve_cascade` matches `resolveCascadeTensor` and vice versa.
pub(crate) fn norm_ident(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase()
}

/// Importance is a SMALL additive tie-break only: the maximum bonus (4.0)
/// stays below the smallest gap between match-kind scores (5), so an exact
/// symbol in a leaf always beats a substring match in a hub.
fn effective_score(h: &FindHit) -> f64 {
    h.score as f64 + h.rank_pct.map_or(0.0, |p| p / 100.0 * 4.0)
}

/// Ranked symbol/file lookup. Match quality always beats everything else:
/// exact symbol > symbol prefix > symbol substring > file-name matches;
/// within one match kind, the more-imported file wins.
///
/// Filtering happens IN SQL against the precomputed `norm` columns (exact
/// `=`, prefix `GLOB`, substring `LIKE`) so only candidates cross into Rust —
/// never the whole symbol table. `norm_ident` output is ASCII-alphanumeric
/// only, so the query can carry no GLOB/LIKE metacharacters; the ESCAPE
/// clause is belt-and-braces. Each SQL ORDER BY replicates
/// [`effective_score`] exactly, which makes the per-query LIMIT lossless:
/// the global top `limit` is contained in the union of the two per-table
/// top-`limit` lists.
pub fn find(conn: &Connection, query: &str, limit: usize) -> anyhow::Result<Vec<FindHit>> {
    ensure_schema(conn)?;
    let q = norm_ident(query);
    if q.is_empty() {
        // A query with no alphanumeric content matches nothing meaningful.
        return Ok(Vec::new());
    }
    let prefix = format!("{q}*");
    let substring = format!("%{q}%");
    let mut hits: Vec<FindHit> = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT s.name, s.kind, s.start_line, s.end_line, f.path, f.rank_pct,
                CASE WHEN s.norm = ?1 THEN 100
                     WHEN s.norm GLOB ?2 THEN 60
                     ELSE 30 END AS score
         FROM symbols s JOIN code_files f ON f.id = s.file_id
         WHERE s.norm = ?1 OR s.norm GLOB ?2 OR s.norm LIKE ?3 ESCAPE '\\'
         ORDER BY score + COALESCE(f.rank_pct, 0.0) / 25.0 DESC, f.path ASC
         LIMIT ?4",
    )?;
    let rows = stmt.query_map(rusqlite::params![q, prefix, substring, limit as i64], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)? as usize,
            r.get::<_, i64>(3)? as usize,
            r.get::<_, String>(4)?,
            r.get::<_, Option<f64>>(5)?,
            r.get::<_, i64>(6)?,
        ))
    })?;
    for (name, kind, start, end, path, rank_pct, score) in rows.filter_map(Result::ok) {
        let lines = end.saturating_sub(start) + 1;
        hits.push(FindHit {
            path,
            name: Some(name),
            kind: Some(kind),
            start_line: start,
            end_line: end,
            score,
            token_estimate: (lines as u64) * 12, // ~12 tokens/line heuristic
            rank_pct,
        });
    }
    let mut fstmt = conn.prepare(
        "SELECT path, rank_pct,
                CASE WHEN norm_stem = ?1 THEN 50
                     WHEN norm_stem GLOB ?2 THEN 25
                     ELSE 10 END AS score
         FROM code_files
         WHERE norm_stem = ?1 OR norm_stem GLOB ?2 OR norm_path LIKE ?3 ESCAPE '\\'
         ORDER BY score + COALESCE(rank_pct, 0.0) / 25.0 DESC, path ASC
         LIMIT ?4",
    )?;
    let frows = fstmt.query_map(rusqlite::params![q, prefix, substring, limit as i64], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<f64>>(1)?, r.get::<_, i64>(2)?))
    })?;
    for (path, rank_pct, score) in frows.filter_map(Result::ok) {
        hits.push(FindHit {
            path,
            name: None,
            kind: None,
            start_line: 1,
            end_line: 1,
            score,
            token_estimate: 0,
            rank_pct,
        });
    }
    hits.sort_by(|a, b| {
        effective_score(b).total_cmp(&effective_score(a)).then_with(|| a.path.cmp(&b.path))
    });
    hits.truncate(limit);
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Platform-agnostic path-suffix assertion: the code index stores
    /// OS-NATIVE absolute paths (a Windows user wants `C:\repo\src\x.rs`,
    /// not a unixism), so a test may not compare against a `/`-joined
    /// literal — it would pass on unix and fail on Windows for a correct
    /// index. Compare component-wise instead.
    fn ends_with_path(haystack: &str, suffix: &str) -> bool {
        let h: Vec<_> = std::path::Path::new(haystack).components().collect();
        let s: Vec<_> = std::path::Path::new(suffix).components().collect();
        h.len() >= s.len() && h[h.len() - s.len()..] == s[..]
    }

    #[test]
    fn file_name_keys_are_taken_with_the_platform_separator() {
        // A code root is walked as absolute OS paths. On unix the two forms
        // agree; on Windows only `Path::file_name` finds the basename.
        let (stem, full) = norm_keys("/repo/src/main.rs");
        assert_eq!(stem, "main");
        assert_eq!(full, norm_ident("/repo/src/main.rs"));
        assert_eq!(
            Path::new(r"C:\repo\src\main.rs")
                .file_name()
                .and_then(|s| s.to_str())
                .map(|b| b.rsplit_once('.').map_or(b, |(s, _)| s)),
            if cfg!(windows) { Some("main") } else { Some(r"C:\repo\src\main") },
            "the platform decides what a backslash means"
        );
    }

    #[test]
    fn rust_symbols_have_exact_tree_ranges() {
        let src = "pub fn alpha() {\n    let x = 1;\n}\n\nstruct Beta {\n    field: u8,\n}\n";
        let syms = extract(Path::new("x.rs"), src).unwrap();
        assert_eq!(
            syms,
            vec![
                Symbol { name: "alpha".into(), kind: "function_item".into(), start_line: 1, end_line: 3 },
                Symbol { name: "Beta".into(), kind: "struct_item".into(), start_line: 5, end_line: 7 },
            ]
        );
    }

    #[test]
    fn typescript_exported_and_class_members() {
        let src = "export function gamma(): void {}\nclass Delta {\n  method_one() {\n    return 1;\n  }\n}\n";
        let syms = extract(Path::new("x.ts"), src).unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"gamma"), "export wrapper must be descended: {names:?}");
        assert!(names.contains(&"Delta"));
        assert!(names.contains(&"method_one"));
        let m = syms.iter().find(|s| s.name == "method_one").unwrap();
        assert_eq!((m.start_line, m.end_line), (3, 5));
    }

    #[test]
    fn python_and_go_and_unsupported() {
        let py = extract(Path::new("a.py"), "def foo():\n    pass\n\nclass Bar:\n    pass\n").unwrap();
        assert_eq!(py.len(), 2);
        let go = extract(Path::new("a.go"), "package m\n\nfunc Baz() {\n}\n").unwrap();
        assert_eq!(go[0].name, "Baz");
        assert!(extract(Path::new("a.zig"), "fn x() {}").is_none(), "unsupported = not measured");
    }

    #[test]
    fn scan_find_end_to_end_with_incremental_resync() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("lib.rs");
        std::fs::write(&f, "fn resolve_cascade() {}\nfn other() {}\n").unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        let r1 = scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();
        assert_eq!((r1.files, r1.symbols), (1, 2));

        let hits = find(&conn, "resolve_cascade", 10).unwrap();
        assert_eq!(hits[0].name.as_deref(), Some("resolve_cascade"));
        assert_eq!(hits[0].score, 100);
        assert_eq!(hits[0].start_line, 1);

        // Unchanged file re-scan keeps rows without re-parsing…
        let r2 = scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();
        assert_eq!(r2.symbols, 0, "unchanged file must not be re-parsed");
        assert_eq!(find(&conn, "resolve", 10).unwrap()[0].score, 60);

        // …an edit re-parses, a deletion removes rows.
        std::fs::write(&f, "fn resolve_cascade_tensor() {}\n").unwrap();
        scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();
        assert!(find(&conn, "other", 10).unwrap().is_empty());
        std::fs::remove_file(&f).unwrap();
        scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();
        assert!(find(&conn, "resolve", 10).unwrap().is_empty());
    }

    #[test]
    fn find_matches_across_naming_conventions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("m.rs"), "fn resolveCascadeTensor() {}\n").unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();
        let hits = find(&conn, "resolve_cascade", 10).unwrap();
        assert_eq!(hits[0].name.as_deref(), Some("resolveCascadeTensor"));
        assert_eq!(hits[0].score, 60, "snake_case query prefix-matches camelCase symbol");
        assert_eq!(find(&conn, "RESOLVE-CASCADE-TENSOR", 10).unwrap()[0].score, 100);
    }

    #[test]
    fn find_importance_is_only_a_tie_break() {
        let dir = tempfile::tempdir().unwrap();
        // alpha.rs imports zebra.rs; main.rs imports both — zebra is the hub
        // (2 in-links), alpha the leaf (1). Without the importance bonus the
        // path tie-break would put alpha.rs before zebra.rs on equal scores.
        std::fs::write(dir.path().join("main.rs"), "mod alpha;\nmod zebra;\nfn main() {}\n").unwrap();
        std::fs::write(dir.path().join("alpha.rs"), "use crate::zebra::zfun;\nfn cascade() {}\nfn cascade_two() {}\n")
            .unwrap();
        std::fs::write(dir.path().join("zebra.rs"), "fn cascade_one() {}\nfn zfun() {}\n").unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();

        let hits = find(&conn, "cascade", 10).unwrap();
        // Exact match in the LEAF still beats prefix matches in the hub.
        assert_eq!(hits[0].name.as_deref(), Some("cascade"));
        assert_eq!(hits[0].score, 100);
        // Equal-kind matches: the hub's higher percentile breaks the tie.
        assert_eq!(hits[1].name.as_deref(), Some("cascade_one"), "hub wins the 60-60 tie: {:?}",
            hits.iter().map(|h| (h.path.clone(), h.name.clone(), h.score)).collect::<Vec<_>>());
        assert!(hits[1].path.ends_with("zebra.rs"));
        assert_eq!(hits[2].name.as_deref(), Some("cascade_two"));
        assert!(hits[1].rank_pct.unwrap() > hits[2].rank_pct.unwrap());
    }

    #[test]
    fn find_substring_and_path_matches_survive_the_sql_filter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("velmurano")).unwrap();
        std::fs::write(dir.path().join("m.rs"), "fn pre_olvecas_post() {}\n").unwrap();
        std::fs::write(dir.path().join("velmurano/inner.rs"), "fn unrelated() {}\n").unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();
        // Symbol substring: 30.
        let hits = find(&conn, "olvecas", 10).unwrap();
        assert_eq!(hits[0].name.as_deref(), Some("pre_olvecas_post"));
        assert_eq!(hits[0].score, 30);
        // Path substring (directory name, not the stem): 10.
        let hits = find(&conn, "velmurano", 10).unwrap();
        let file_hit = hits.iter().find(|h| h.name.is_none()).expect("file hit listed");
        assert!(ends_with_path(&file_hit.path, "velmurano/inner.rs"));
        assert_eq!(file_hit.score, 10);
        // No match at all returns nothing (SQL-side filtering).
        assert!(find(&conn, "zzznothing", 10).unwrap().is_empty());
        assert!(find(&conn, "---", 10).unwrap().is_empty(), "no-alphanumeric query matches nothing");
    }

    #[test]
    fn parallel_scan_indexes_a_wide_tree_correctly() {
        let dir = tempfile::tempdir().unwrap();
        for d in 0..6 {
            let sub = dir.path().join(format!("mod{d}/nested"));
            std::fs::create_dir_all(&sub).unwrap();
            for f in 0..10 {
                std::fs::write(
                    sub.join(format!("file{f}.rs")),
                    format!("fn sym_{d}_{f}() {{}}\nfn helper_{d}_{f}() {{}}\n"),
                )
                .unwrap();
            }
        }
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        let r1 = scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();
        assert_eq!((r1.files, r1.symbols), (60, 120));
        let hits = find(&conn, "sym_3_7", 5).unwrap();
        assert_eq!(hits[0].score, 100);
        assert!(ends_with_path(&hits[0].path, "mod3/nested/file7.rs"));
        // Unchanged rescan parses nothing; counts stay stable.
        let r2 = scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();
        assert_eq!((r2.files, r2.symbols), (60, 0));
        // Deletions prune rows for exactly the vanished files.
        for f in 0..5 {
            std::fs::remove_file(dir.path().join(format!("mod0/nested/file{f}.rs"))).unwrap();
        }
        let r3 = scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();
        assert_eq!(r3.files, 55);
        assert!(find(&conn, "sym_0_2", 5).unwrap().iter().all(|h| h.name.is_none()));
        assert_eq!(file_count(&conn).unwrap(), 55);
    }

    #[test]
    fn find_ranks_symbol_matches_above_file_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cascade.rs"), "fn unrelated() {}\n").unwrap();
        std::fs::write(dir.path().join("m.rs"), "fn cascade() {}\n").unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();
        let hits = find(&conn, "cascade", 10).unwrap();
        assert_eq!(hits[0].name.as_deref(), Some("cascade"), "exact symbol beats file name");
        assert!(hits.iter().any(|h| h.name.is_none()), "file hit still listed");
    }
}
