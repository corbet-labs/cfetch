//! The code index (Milestone 3): tree-sitter symbol extraction over the
//! configured code roots, stored in the same per-host SQLite cache, served by
//! `cfetch find` with exact line ranges — read one function instead of a file.
//!
//! Symbol boundaries come from the syntax tree, never from regex line
//! guessing (measured upstream: 97% of guessed end lines were wrong). Whole
//! files only, no partial reparse. tree-sitter recovers from broken syntax
//! instead of refusing it, so a damaged file still contributes symbols and
//! there is no "failed to parse" to observe — what the file yields is
//! recorded as an incomplete measurement instead, and re-measured until it
//! reads cleanly.

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use tree_sitter::{Language, Node, Parser};

#[derive(Debug, PartialEq)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    pub start_line: usize, // 1-indexed, inclusive
    pub end_line: usize,   // 1-indexed, inclusive
    /// Start line of the nearest enclosing symbol. `None` means the symbol is
    /// a file-level definition and may be the target of an explicit import.
    pub parent_start_line: Option<usize>,
}

/// A parser-observed use inside one containing symbol. It is deliberately
/// unresolved here: graph resolution later requires an explicit import
/// binding and exactly one file-level target definition.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SymbolUse {
    pub name: String,
    pub qualifier: Option<String>,
    pub relation: String,
    pub start_line: usize,
    pub end_line: usize,
    pub container_start_line: usize,
}

pub struct FindHit {
    pub path: String,
    pub name: Option<String>,
    pub kind: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub score: i64,
    /// Estimated cost of READING this hit: the symbol's lines for a symbol
    /// match, the whole indexed file for a file match. Never zero for a hit
    /// that exists — a free-looking hit defeats any budget built on it.
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

/// Why a symbol list may be shorter than what the file actually declares.
///
/// The list and this reason are separate facts on purpose. An empty list with
/// no gap is a MEASUREMENT — the file parsed and declares nothing — while an
/// empty list with a gap means we do not know what the file declares. A caller
/// that flattens the two reads "we could not look" as "there is nothing
/// there", which is how a symbol that plainly exists on disk turns into an
/// unexplained `no hits`.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Gap {
    /// No grammar ships for this extension; nothing was attempted.
    UnsupportedLanguage,
    /// The grammar would not load, or the parser returned no tree at all.
    ParserUnavailable,
    /// The tree came back carrying error nodes. tree-sitter recovers from
    /// broken syntax rather than failing, so a list is still produced — but
    /// any node spanning the damage has an extent the recovery guessed, and
    /// symbols swallowed by it are missing from the list entirely.
    SyntaxErrors,
}

impl Gap {
    /// Stable short name, persisted per file so a later scan — and an operator
    /// reading the cache — can tell why a file contributed what it did.
    fn slug(self) -> &'static str {
        match self {
            Gap::UnsupportedLanguage => "unsupported-language",
            Gap::ParserUnavailable => "parser-unavailable",
            Gap::SyntaxErrors => "syntax-errors",
        }
    }
}

/// The outcome of looking at one file: what the tree proved, and what kept
/// that list from being the whole truth.
#[derive(Debug, PartialEq)]
pub struct Extraction {
    /// Every named symbol the tree yielded. On a clean parse each extent is
    /// exact and the list is the file's whole symbol table; where `gap` is
    /// set, neither holds — see [`Gap`].
    pub symbols: Vec<Symbol>,
    /// Direct calls and type references proved by the language grammar.
    pub(crate) uses: Vec<SymbolUse>,
    /// `None` when the list is complete; see [`Gap`].
    pub gap: Option<Gap>,
}

impl Extraction {
    fn unavailable(gap: Gap) -> Self {
        Extraction { symbols: Vec::new(), uses: Vec::new(), gap: Some(gap) }
    }
}

/// Extracts named symbols from source text.
///
/// Everything the recovery produced is reported, damaged file or not. Dropping
/// the symbols under an error node was tried and rejected: measured on broken
/// Rust, TypeScript and Python, `Node::has_error` flags an enclosing function
/// or class whose own extent is exactly right just because something inside it
/// is broken, so the rule costs more correct symbols than it catches wrong
/// ones — and a symbol missing from `find` reads to an agent as an
/// authoritative "no such thing", while an extent a few lines too long is
/// visible the moment the lines are read. The damage is reported at the file
/// level instead, where the caller can act on it.
pub fn extract(path: &Path, source: &str) -> Extraction {
    let Some(lang) = lang_of(path) else {
        return Extraction::unavailable(Gap::UnsupportedLanguage);
    };
    let mut parser = Parser::new();
    if parser.set_language(&language(&lang)).is_err() {
        return Extraction::unavailable(Gap::ParserUnavailable);
    }
    let Some(tree) = parser.parse(source, None) else {
        return Extraction::unavailable(Gap::ParserUnavailable);
    };
    let kinds = symbol_kinds(&lang);
    let src = source.as_bytes();
    let mut out = Vec::new();
    let root = tree.root_node();
    walk_symbols(root, kinds, src, None, &mut out);
    let mut uses = Vec::new();
    walk_uses(root, &lang, kinds, src, None, &mut uses);
    Extraction { symbols: out, uses, gap: root.has_error().then_some(Gap::SyntaxErrors) }
}

fn walk_symbols(
    node: Node,
    kinds: &[&str],
    src: &[u8],
    parent_start_line: Option<usize>,
    out: &mut Vec<Symbol>,
) {
    let mut child_parent = parent_start_line;
    if kinds.contains(&node.kind())
        && let Some(name) = node_name(&node, src)
    {
        let start_line = node.start_position().row + 1;
        out.push(Symbol {
            name,
            kind: node.kind().to_string(),
            start_line,
            end_line: node.end_position().row + 1,
            parent_start_line,
        });
        child_parent = Some(start_line);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_symbols(child, kinds, src, child_parent, out);
    }
}

fn qualified_name(node: Node, src: &[u8]) -> Option<(Option<String>, String)> {
    let text = node.utf8_text(src).ok()?.trim();
    let parts: Vec<&str> = text
        .split([':', '.'])
        .filter(|part| !part.is_empty())
        .collect();
    match parts.as_slice() {
        [name] if name.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '$')) => {
            Some((None, (*name).to_string()))
        }
        [qualifier, name]
            if qualifier.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '$'))
                && name.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '$')) =>
        {
            Some((Some((*qualifier).to_string()), (*name).to_string()))
        }
        _ => None,
    }
}

fn call_target(node: Node, lang: &Lang, src: &[u8]) -> Option<(Option<String>, String)> {
    let expected = match lang {
        Lang::Python => "call",
        _ => "call_expression",
    };
    if node.kind() != expected {
        return None;
    }
    qualified_name(node.child_by_field_name("function")?, src)
}

fn type_reference(node: Node, lang: &Lang, src: &[u8]) -> Option<(Option<String>, String)> {
    let is_type = match lang {
        Lang::Rust => matches!(node.kind(), "type_identifier" | "scoped_type_identifier"),
        Lang::TypeScript | Lang::Tsx => {
            matches!(node.kind(), "type_identifier" | "nested_type_identifier")
        }
        Lang::Go => matches!(node.kind(), "type_identifier" | "qualified_type"),
        // These grammars do not distinguish a safe imported-symbol reference
        // from an ordinary identifier. No edge is better than a guessed one.
        Lang::JavaScript | Lang::Python => false,
    };
    is_type.then(|| qualified_name(node, src)).flatten()
}

fn walk_uses(
    node: Node,
    lang: &Lang,
    kinds: &[&str],
    src: &[u8],
    container_start_line: Option<usize>,
    out: &mut Vec<SymbolUse>,
) {
    let container_start_line = if kinds.contains(&node.kind()) && node_name(&node, src).is_some() {
        Some(node.start_position().row + 1)
    } else {
        container_start_line
    };
    if let Some(container_start_line) = container_start_line {
        let observed = call_target(node, lang, src)
            .map(|target| ("calls", target))
            .or_else(|| type_reference(node, lang, src).map(|target| ("references", target)));
        if let Some((relation, (qualifier, name))) = observed {
            out.push(SymbolUse {
                name,
                qualifier,
                relation: relation.to_string(),
                start_line: node.start_position().row + 1,
                end_line: node.end_position().row + 1,
                container_start_line,
            });
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_uses(child, lang, kinds, src, container_start_line, out);
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

/// Cache key that no real file can present, written over rows whose symbols
/// predate gap tracking so the next scan re-measures them. Invalidating the
/// key rather than deleting the rows keeps `find` answering from the old
/// snapshot until the replacement exists.
const REMEASURE: i64 = -1;

pub fn ensure_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS code_files(
           id INTEGER PRIMARY KEY,
           path TEXT UNIQUE NOT NULL,
           mtime INTEGER NOT NULL,
           size INTEGER NOT NULL,
           rank_pct REAL,
           norm_stem TEXT NOT NULL DEFAULT '',
           norm_path TEXT NOT NULL DEFAULT '',
           unmeasured TEXT
         );",
    )?;
    // A cache built before the column existed cannot say which of its rows
    // are complete, and a row that merely LOOKS complete is the one thing the
    // shortcut below must never trust — so the column arrives with every
    // existing row's cache key invalidated, and one scan sorts them out. The
    // guard matters because the read-only query connections reach
    // ensure_schema too and must find nothing left to write.
    let tracked = conn
        .prepare("SELECT 1 FROM pragma_table_info('code_files') WHERE name='unmeasured'")?
        .exists([])?;
    if !tracked {
        conn.execute_batch(&format!(
            "ALTER TABLE code_files ADD COLUMN unmeasured TEXT;
             UPDATE code_files SET mtime = {REMEASURE};"
        ))?;
    }
    let symbol_schema_current = conn
        .prepare("SELECT 1 FROM pragma_table_info('symbols') WHERE name='parent_start_line'")?
        .exists([])?;
    if !symbol_schema_current {
        // This index is a disposable cache. Rebuild old rows so parentage and
        // uses can never silently describe different parses.
        conn.execute_batch(&format!(
            "DROP TABLE IF EXISTS symbol_uses;
             DROP TABLE IF EXISTS symbols;
             UPDATE code_files SET mtime = {REMEASURE};"
        ))?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS symbols(
           id INTEGER PRIMARY KEY,
           file_id INTEGER NOT NULL REFERENCES code_files(id) ON DELETE CASCADE,
           name TEXT NOT NULL,
           kind TEXT NOT NULL,
           norm TEXT NOT NULL DEFAULT '',
           start_line INTEGER NOT NULL,
           end_line INTEGER NOT NULL,
           parent_start_line INTEGER
         );
         CREATE TABLE IF NOT EXISTS symbol_uses(
           file_id INTEGER NOT NULL REFERENCES code_files(id) ON DELETE CASCADE,
           name TEXT NOT NULL,
           qualifier TEXT,
           relation TEXT NOT NULL,
           start_line INTEGER NOT NULL,
           end_line INTEGER NOT NULL,
           container_start_line INTEGER NOT NULL,
           PRIMARY KEY(file_id, relation, start_line, end_line, name, container_start_line)
         );
         CREATE INDEX IF NOT EXISTS symbols_norm ON symbols(norm);
         CREATE INDEX IF NOT EXISTS symbol_uses_file ON symbol_uses(file_id);
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
    /// Freshly measured content to (re)insert, with whatever kept the symbol
    /// list from being complete.
    Parsed {
        path: String,
        mtime: i64,
        size: i64,
        symbols: Vec<Symbol>,
        uses: Vec<SymbolUse>,
        gap: Option<Gap>,
        imports: Vec<crate::graph::ImportFact>,
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
    scan_code_in(conn, roots, &crate::paths::default_brain_root())
}

/// Same scan with an explicit brain root, so the secrets/exhaust bar can be
/// tested without mutating the process environment.
pub fn scan_code_in(
    conn: &mut Connection,
    roots: &[PathBuf],
    brain_root: &Path,
) -> anyhow::Result<CodeScanReport> {
    ensure_schema(conn)?;
    let mut report = CodeScanReport { files: 0, symbols: 0, edges: 0 };
    // Known (mtime, size) per path, read once — the walker threads make the
    // skip decision without touching the DB.
    //
    // Only rows with a complete measurement qualify. A file we could not fully
    // read is left out on purpose: its gap may be OURS, not the file's — a
    // grammar that gains the syntax it choked on completes the file without a
    // byte of it changing — and the (mtime, size) shortcut would otherwise
    // make that blindness permanent. The re-parse cost is bounded by the
    // number of files that actually failed, which is near zero in a tree the
    // grammars understand.
    let known: std::collections::HashMap<String, (i64, i64)> = {
        let mut stmt =
            conn.prepare("SELECT path, mtime, size FROM code_files WHERE unmeasured IS NULL")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))
        })?;
        rows.filter_map(Result::ok).collect()
    };
    let tx = conn.transaction()?;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    // The secrets/exhaust bar holds for the code index too, at ANY
    // configuration: pointing code_roots at — or beneath — the brain's
    // hard-excluded prefixes must refuse them, not index them, and a
    // rescan purges whatever an earlier configuration let in (rows the
    // walker never sees are stale and deleted below). The comparison is
    // component-wise (`Path::starts_with`), so a root outside the brain
    // can never match by string accident.
    let refused: [std::path::PathBuf; 2] = [
        brain_root.join("mind").join("secrets"),
        brain_root.join("logs"),
    ];
    for root in roots {
        if refused.iter().any(|p| root.starts_with(p)) {
            continue;
        }
        let refused = refused.clone();
        let (msg_tx, msg_rx) = std::sync::mpsc::channel::<WalkMsg>();
        let known = &known;
        std::thread::scope(|s| -> anyhow::Result<()> {
            s.spawn(move || {
                crate::index::tree_walker(root)
                    .filter_entry(move |e| {
                        e.file_name().to_str().map(|n| !skip_dir(n)).unwrap_or(true)
                            && !refused.iter().any(|p| e.path().starts_with(p))
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
                            let Extraction { symbols, uses, gap } = extract(entry.path(), &source);
                            // Import edges refresh with the file that declares
                            // them; targets added later self-heal on the
                            // importer's next change.
                            let imports = crate::graph::extract_imports(entry.path(), &source, root);
                            let _ = msg_tx.send(WalkMsg::Parsed {
                                path,
                                mtime,
                                size,
                                symbols,
                                uses,
                                gap,
                                imports,
                            });
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
                    WalkMsg::Parsed { path, mtime, size, symbols, uses, gap, imports } => {
                        crate::graph::replace_file_edges(&tx, &path, &imports)?;
                        // prepare_cached: these two run once per changed file
                        // and once per symbol — re-preparing them would make
                        // the single writer the bottleneck under the parallel
                        // parse workers.
                        tx.prepare_cached("DELETE FROM code_files WHERE path=?1")?
                            .execute([&path])?;
                        let (norm_stem, norm_path) = norm_keys(&path);
                        tx.prepare_cached(
                            "INSERT INTO code_files(path, mtime, size, norm_stem, norm_path,
                                                    unmeasured)
                             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
                        )?
                        .execute(rusqlite::params![
                            path,
                            mtime,
                            size,
                            norm_stem,
                            norm_path,
                            gap.map(Gap::slug)
                        ])?;
                        let file_id = tx.last_insert_rowid();
                        let mut ins = tx.prepare_cached(
                            "INSERT INTO symbols(file_id, name, kind, norm, start_line, end_line,
                                                 parent_start_line)
                             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        )?;
                        for s in &symbols {
                            ins.execute(rusqlite::params![
                                file_id,
                                s.name,
                                s.kind,
                                norm_ident(&s.name),
                                s.start_line as i64,
                                s.end_line as i64,
                                s.parent_start_line.map(|line| line as i64)
                            ])?;
                        }
                        drop(ins);
                        // Two AST nodes can yield one identical use tuple -
                        // e.g. two calls to the same function written on a
                        // single line inside the same container
                        // (`f(a); f(b);`): same relation, start/end line,
                        // name and container, distinguished only by byte
                        // offsets the table does not store. The primary key
                        // is correct; the duplicate carries no information
                        // and is dropped at the insert.
                        let mut ins_use = tx.prepare_cached(
                            "INSERT OR IGNORE INTO symbol_uses(file_id, name, qualifier, relation, start_line,
                                                     end_line, container_start_line)
                             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        )?;
                        for u in &uses {
                            ins_use.execute(rusqlite::params![
                                file_id,
                                u.name,
                                u.qualifier,
                                u.relation,
                                u.start_line as i64,
                                u.end_line as i64,
                                u.container_start_line as i64,
                            ])?;
                        }
                        drop(ins_use);
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
    report.edges = tx.query_row(
        "SELECT count(*) FROM (SELECT DISTINCT src, dst FROM import_edges)",
        [],
        |r| r.get::<_, i64>(0),
    )? as usize;
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
        "SELECT path, size, rank_pct,
                CASE WHEN norm_stem = ?1 THEN 50
                     WHEN norm_stem GLOB ?2 THEN 25
                     ELSE 10 END AS score
         FROM code_files
         WHERE norm_stem = ?1 OR norm_stem GLOB ?2 OR norm_path LIKE ?3 ESCAPE '\\'
         ORDER BY score + COALESCE(rank_pct, 0.0) / 25.0 DESC, path ASC
         LIMIT ?4",
    )?;
    let frows = fstmt.query_map(rusqlite::params![q, prefix, substring, limit as i64], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<f64>>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    for (path, size, rank_pct, score) in frows.filter_map(Result::ok) {
        hits.push(FindHit {
            path,
            name: None,
            kind: None,
            start_line: 1,
            end_line: 1,
            score,
            // A file match means "open this whole file", so its cost is the
            // file. The constant 0 that stood here priced that as free, and
            // every budget reading `token_estimate` believed it.
            token_estimate: crate::hook_io::estimate_tokens(size.max(0) as usize),
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
        let syms = extract(Path::new("x.rs"), src).symbols;
        assert_eq!(
            syms,
            vec![
                Symbol { name: "alpha".into(), kind: "function_item".into(), start_line: 1, end_line: 3, parent_start_line: None },
                Symbol { name: "Beta".into(), kind: "struct_item".into(), start_line: 5, end_line: 7, parent_start_line: None },
            ]
        );
    }

    #[test]
    fn the_secrets_bar_holds_for_the_code_index_at_any_configuration() {
        // code_roots pointed straight AT the secrets prefix, at a parent
        // containing it, and at an unrelated tree — the bar must refuse
        // the prefix in every shape, and never by name accident elsewhere.
        let brain = tempfile::tempdir().unwrap();
        let secrets = brain.path().join("mind").join("secrets");
        std::fs::create_dir_all(&secrets).unwrap();
        std::fs::write(secrets.join("vault.rs"), "pub fn api_key() -> &'static str { \"MEINGEHEIMNIS-4711\" }\n").unwrap();
        std::fs::create_dir_all(brain.path().join("src")).unwrap();
        std::fs::write(brain.path().join("src/main.rs"), "pub fn visible() {}\n").unwrap();

        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();

        // Root AT the prefix: nothing indexed, no error, no rows.
        let r1 = scan_code_in(&mut conn, std::slice::from_ref(&secrets), brain.path()).unwrap();
        assert_eq!(r1.files, 0, "the whole root is behind the bar");
        // Root at the brain itself: the prefix is pruned, the real code is not.
        let r2 = scan_code_in(&mut conn, &[brain.path().to_path_buf()], brain.path()).unwrap();
        assert_eq!(r2.files, 1);
        let rows: Vec<String> = conn
            .prepare("SELECT path FROM code_files")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(rows.iter().any(|p| ends_with_path(p, "src/main.rs")));
        assert!(!rows.iter().any(|p| p.contains("secrets") || p.contains("vault")), "no secret rows: {rows:?}");
    }

    #[test]
    fn a_rescan_purges_secret_rows_an_earlier_configuration_let_in() {
        // The bar must also HEAL: rows indexed before the refusal existed
        // are stale once the walker refuses to see them, and the scan's
        // gone-from-disk pass deletes them.
        let brain = tempfile::tempdir().unwrap();
        let secrets = brain.path().join("mind").join("secrets");
        std::fs::create_dir_all(&secrets).unwrap();
        std::fs::write(secrets.join("vault.rs"), "pub fn api_key() -> &'static str { \"sk-test-XYZ\" }\n").unwrap();
        std::fs::create_dir_all(brain.path().join("src")).unwrap();
        std::fs::write(brain.path().join("src/main.rs"), "pub fn visible() {}\n").unwrap();

        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();

        // First scan creates the schema and indexes the visible file.
        scan_code_in(&mut conn, &[brain.path().to_path_buf()], brain.path()).unwrap();
        // Simulate the pre-fix state: a secret row already in the index.
        conn.execute(
            "INSERT INTO code_files(path, mtime, size, norm_stem, norm_path) VALUES(?1, 0, 0, 'vault', 'vault')",
            [secrets.join("vault.rs").to_string_lossy().to_string()],
        )
        .unwrap();

        let _ = scan_code_in(&mut conn, &[brain.path().to_path_buf()], brain.path()).unwrap();

        let rows: Vec<String> = conn
            .prepare("SELECT path FROM code_files")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(rows.len(), 1);
        assert!(ends_with_path(&rows[0], "src/main.rs"), "stale secret row purged: {rows:?}");
    }

    #[test]
    fn a_file_root_inside_the_prefix_is_refused_too() {
        let brain = tempfile::tempdir().unwrap();
        let secrets = brain.path().join("mind").join("secrets");
        std::fs::create_dir_all(&secrets).unwrap();
        let vault = secrets.join("vault.rs");
        std::fs::write(&vault, "pub fn api_key() -> &'static str { \"MEINGEHEIMNIS\" }\n").unwrap();

        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        let r = scan_code_in(&mut conn, &[vault], brain.path()).unwrap();
        assert_eq!(r.files, 0, "a bare file root under the prefix is behind the bar");
    }

    #[test]
    fn typescript_exported_and_class_members() {
        let src = "export function gamma(): void {}\nclass Delta {\n  method_one() {\n    return 1;\n  }\n}\n";
        let syms = extract(Path::new("x.ts"), src).symbols;
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"gamma"), "export wrapper must be descended: {names:?}");
        assert!(names.contains(&"Delta"));
        assert!(names.contains(&"method_one"));
        let m = syms.iter().find(|s| s.name == "method_one").unwrap();
        assert_eq!((m.start_line, m.end_line), (3, 5));
    }

    #[test]
    fn python_and_go_and_unsupported() {
        let py = extract(Path::new("a.py"), "def foo():\n    pass\n\nclass Bar:\n    pass\n");
        assert_eq!(py.symbols.len(), 2);
        assert_eq!(py.gap, None);
        let go = extract(Path::new("a.go"), "package m\n\nfunc Baz() {\n}\n");
        assert_eq!(go.symbols[0].name, "Baz");
    }

    #[test]
    fn parser_uses_keep_direct_and_qualified_calls_without_name_guessing() {
        for (path, source, expected) in [
            (
                "x.rs",
                "fn caller(input: Thing) { run(); worker::go(); }\n",
                vec![("calls", None, "run"), ("calls", Some("worker"), "go"), ("references", None, "Thing")],
            ),
            (
                "x.ts",
                "function caller(input: Thing) { run(); worker.go(); }\n",
                vec![("calls", None, "run"), ("calls", Some("worker"), "go"), ("references", None, "Thing")],
            ),
            (
                "x.go",
                "package x\nfunc caller(input Thing) { run(); worker.Go() }\n",
                vec![("calls", None, "run"), ("calls", Some("worker"), "Go"), ("references", None, "Thing")],
            ),
        ] {
            let extraction = extract(Path::new(path), source);
            let actual: Vec<(&str, Option<&str>, &str)> = extraction
                .uses
                .iter()
                .map(|usage| {
                    (
                        usage.relation.as_str(),
                        usage.qualifier.as_deref(),
                        usage.name.as_str(),
                    )
                })
                .collect();
            for use_fact in expected {
                assert!(actual.contains(&use_fact), "{path}: missing {use_fact:?} in {actual:?}");
            }
        }
    }

    #[test]
    fn nested_symbols_retain_their_enclosing_definition() {
        let extraction = extract(
            Path::new("x.ts"),
            "class Outer {\n  method() { return 1; }\n}\n",
        );
        let outer = extraction.symbols.iter().find(|symbol| symbol.name == "Outer").unwrap();
        let method = extraction.symbols.iter().find(|symbol| symbol.name == "method").unwrap();
        assert_eq!(outer.parent_start_line, None);
        assert_eq!(method.parent_start_line, Some(outer.start_line));
    }

    #[test]
    fn nothing_to_look_with_is_not_the_same_as_nothing_there() {
        // Both hand back an empty list; only the reason separates them, and
        // without it a caller cannot tell an unreadable file from an empty one.
        let unsupported = extract(Path::new("a.zig"), "fn x() {}");
        assert_eq!(unsupported.gap, Some(Gap::UnsupportedLanguage), "unsupported = not measured");
        assert!(unsupported.symbols.is_empty());
        let empty = extract(Path::new("a.rs"), "// nothing but a comment\n");
        assert_eq!(empty.gap, None, "a file that declares nothing IS a measurement");
        assert!(empty.symbols.is_empty());
    }

    #[test]
    fn a_damaged_file_reports_short_instead_of_passing_as_complete() {
        let names = |e: &Extraction| -> Vec<String> {
            e.symbols.iter().map(|s| s.name.clone()).collect()
        };
        // `Alpha`'s brace never closes, so recovery folds `gamma` into the
        // struct. The flag is not decoration: the list really is short of what
        // the file declares, and nothing in the list itself shows that.
        let broken = extract(Path::new("x.rs"), "struct Alpha {\n    a: u8,\n\nfn gamma() {}\n");
        assert_eq!(broken.gap, Some(Gap::SyntaxErrors), "damage must be reported, not absorbed");
        assert_eq!(names(&broken), ["Alpha"], "a swallowed symbol is simply gone");
        let whole = extract(Path::new("x.rs"), "struct Alpha {\n    a: u8,\n}\nfn gamma() {}\n");
        assert_eq!(whole.gap, None, "the same file, closed, is a complete measurement");
        assert_eq!(names(&whole), ["Alpha", "gamma"]);
    }

    #[test]
    fn scan_survives_two_same_name_calls_on_one_line() {
        // Two calls to the same function written on a single line inside one
        // named container produce two identical (relation, lines, name,
        // container) tuples - the primary key holds, the second row must be
        // dropped, and the scan must not abort. Found by pointing the code
        // index at lodash (fp/_baseConvert.js style ternary helpers).
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.js");
        std::fs::write(
            &f,
            "function g(f, n) {\n  return n == 2 ? function(a, b) { return f(a, b); } : function(a) { return f(a); };\n}\n",
        )
        .unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        let r = scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();
        assert_eq!(r.files, 1, "the file must index, not crash");
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
    fn a_file_we_could_not_fully_read_is_re_measured_even_when_nothing_changed() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("broken.rs");
        // Same length, same mtime: the incremental shortcut cannot tell these
        // two revisions apart. That is deliberate — the real trigger is a
        // grammar upgrade, where the file on disk does not change at all and
        // only our ability to read it does.
        let broken = "fn alpha() {\n    if q {    \n}\n";
        let fixed = "fn alpha() {\n    let q = 1;\n}\n";
        assert_eq!(broken.len(), fixed.len(), "the shortcut is keyed on (mtime, size)");
        let stamp = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let write_at = |body: &str| {
            std::fs::write(&f, body).unwrap();
            std::fs::File::options().write(true).open(&f).unwrap().set_modified(stamp).unwrap();
        };
        let unmeasured = |c: &Connection| {
            c.query_row("SELECT unmeasured FROM code_files", [], |r| r.get::<_, Option<String>>(0))
                .unwrap()
        };

        write_at(broken);
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();
        assert!(
            find(&conn, "alpha", 10).unwrap().iter().all(|h| h.name.is_none()),
            "recovery salvaged no symbol from the damaged function"
        );
        assert_eq!(
            unmeasured(&conn),
            Some("syntax-errors".into()),
            "why the file gave nothing travels with the file"
        );

        write_at(fixed);
        scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();
        let hits = find(&conn, "alpha", 10).unwrap();
        let top = hits.first().expect("a gap must not be permanent: the file was not re-measured");
        assert_eq!(top.name.as_deref(), Some("alpha"));
        assert_eq!((top.start_line, top.end_line), (1, 3));
        assert_eq!(unmeasured(&conn), None, "a complete measurement clears the reason");
    }

    #[test]
    fn a_cache_that_predates_gap_tracking_is_re_measured_not_trusted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("m.rs"), "fn alpha() {}\n").unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();
        let mtime: i64 = conn.query_row("SELECT mtime FROM code_files", [], |r| r.get(0)).unwrap();
        assert!(mtime > 0);

        // Rewind to the pre-gap schema: rows written by a binary that
        // published error-recovered ranges and had nowhere to record it.
        conn.execute_batch("ALTER TABLE code_files DROP COLUMN unmeasured").unwrap();
        ensure_schema(&conn).unwrap();
        assert_eq!(
            conn.query_row("SELECT mtime FROM code_files", [], |r| r.get::<_, i64>(0)).unwrap(),
            REMEASURE,
            "rows of unknown completeness must lose the shortcut"
        );
        // The old answers keep serving until the replacement exists.
        assert_eq!(find(&conn, "alpha", 10).unwrap()[0].name.as_deref(), Some("alpha"));

        scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();
        assert_eq!(
            conn.query_row("SELECT mtime FROM code_files", [], |r| r.get::<_, i64>(0)).unwrap(),
            mtime,
            "the next scan re-measures without the file having changed"
        );
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

    /// Generated or vendored sources a repo tracks on purpose: the overlay
    /// keeps them out of the code index without untracking them, which
    /// `.gitignore` — the only file-based control this walker read before —
    /// could not do.
    #[test]
    fn cfetchignore_scopes_a_subtree_out_of_the_code_index() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("generated")).unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn kept_symbol() {}\n").unwrap();
        std::fs::write(dir.path().join("generated/api.rs"), "fn scoped_out_symbol() {}\n").unwrap();
        std::fs::write(dir.path().join(".cfetchignore"), "generated/\n").unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();

        let report = scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();
        assert_eq!(report.files, 1);
        assert!(find(&conn, "scoped_out_symbol", 5).unwrap().is_empty());
        assert_eq!(find(&conn, "kept_symbol", 5).unwrap()[0].score, 100);
    }

    #[test]
    fn a_file_match_prices_the_whole_file_instead_of_nothing() {
        // A file hit says "open this file". While it reported 0 tokens, every
        // budget reading `token_estimate` was told that opening a megabyte of
        // vendored source was free, and the number printed next to it in JSON
        // was a lie no caller could catch.
        let dir = tempfile::tempdir().unwrap();
        let body = format!("fn velmurano_helper() {{}}\n// {}\n", "padding ".repeat(400));
        std::fs::write(dir.path().join("velmurano.rs"), &body).unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();
        let hits = find(&conn, "velmurano", 10).unwrap();
        let file_hit = hits.iter().find(|h| h.name.is_none()).expect("file hit listed");
        assert_eq!(
            file_hit.token_estimate,
            crate::hook_io::estimate_tokens(body.len()),
            "a file match costs the file"
        );
        // The symbol match in the same file still prices only its own lines.
        let sym = hits.iter().find(|h| h.name.is_some()).unwrap();
        assert!(sym.token_estimate < file_hit.token_estimate);
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
