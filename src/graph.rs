//! The import graph and importance ranking (Milestone 3 remainder): which
//! files does the project itself consider central?
//!
//! Edges are project-internal imports extracted from the HEAD of each file
//! only — the import block, not `use` statements buried in function bodies —
//! and every edge must resolve to a real file inside the same code root.
//! Unresolvable imports produce NO edge: an invented edge poisons the whole
//! ranking, while a missing one merely under-counts (no signal beats a false
//! one). PageRank (damping 0.85, 30 iterations, dangling mass redistributed
//! through the restart vector) turns the edges into per-file importance,
//! stored as a RANK PERCENTILE — raw PageRank is heavily right-skewed, so
//! absolute values mislead while percentiles compare cleanly. Projects with
//! zero resolvable edges get no scores at all.
//!
//! The same graph powers `cfetch map`: a repo overview ordered by
//! personalized PageRank (restart vector seeded by `--focus` matches),
//! fitted to a token budget by binary search over the entry count.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use rusqlite::Connection;

pub const DAMPING: f64 = 0.85;
pub const ITERATIONS: usize = 30;

/// Import statements live at the top of a file; anything beyond this many
/// lines is body, not head.
const HEAD_MAX_LINES: usize = 400;
/// Cap for joining one multi-line import statement.
const JOIN_MAX_LINES: usize = 30;
/// Symbols shown per `cfetch map` line.
const MAP_SYMBOLS_SHOWN: usize = 4;

enum EdgeLang {
    Rust,
    TsJs,
    Python,
    Go,
}

fn edge_lang(path: &Path) -> Option<EdgeLang> {
    match path.extension()?.to_str()? {
        "rs" => Some(EdgeLang::Rust),
        "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" => Some(EdgeLang::TsJs),
        "py" => Some(EdgeLang::Python),
        "go" => Some(EdgeLang::Go),
        _ => None,
    }
}

/// Lexical path normalization (`a/./b/../c` → `a/c`). Deliberately not
/// `canonicalize`: that resolves symlinks and would make edge paths disagree
/// with the walker's paths on link-heavy trees.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Extracts the project-internal import edges of one file. Returned paths are
/// normalized, deduplicated, inside `root`, and never the file itself.
pub fn extract_edges(file: &Path, source: &str, root: &Path) -> Vec<PathBuf> {
    let Some(lang) = edge_lang(file) else { return Vec::new() };
    let head: Vec<&str> = source.lines().take(HEAD_MAX_LINES).collect();
    let raw = match lang {
        EdgeLang::Rust => rust_edges(file, &head, root),
        EdgeLang::TsJs => tsjs_edges(file, &head),
        EdgeLang::Python => python_edges(file, &head),
        EdgeLang::Go => go_edges(file, &head, root),
    };
    let self_path = normalize(file);
    let mut out: BTreeSet<PathBuf> = BTreeSet::new();
    for t in raw {
        let t = normalize(&t);
        if t != self_path && t.starts_with(root) && crate::code::is_indexable(&t) {
            out.insert(t);
        }
    }
    out.into_iter().collect()
}

fn first_file<const N: usize>(candidates: [PathBuf; N]) -> Option<PathBuf> {
    candidates.into_iter().find(|c| c.is_file())
}

/// First single- or double-quoted substring of `s`.
fn quoted(s: &str) -> Option<String> {
    let start = s.find(['\'', '"'])?;
    let q = s.as_bytes()[start] as char;
    let rest = &s[start + 1..];
    let end = rest.find(q)?;
    Some(rest[..end].to_string())
}

// ---------------------------------------------------------------- Rust

/// Strips a leading `pub` / `pub(crate)` etc. visibility qualifier.
fn strip_vis(line: &str) -> &str {
    let Some(rest) = line.strip_prefix("pub") else { return line };
    let rest = rest.trim_start();
    if let Some(group) = rest.strip_prefix('(')
        && let Some(close) = group.find(')')
    {
        return group[close + 1..].trim_start();
    }
    rest
}

fn rust_edges(file: &Path, head: &[&str], root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut in_block_comment = false;
    while i < head.len() {
        let line = head[i].trim();
        i += 1;
        if in_block_comment {
            if line.contains("*/") {
                in_block_comment = false;
            }
            continue;
        }
        if line.is_empty() || line.starts_with("//") || line.starts_with("#[") || line.starts_with("#!") {
            continue;
        }
        if line.starts_with("/*") {
            in_block_comment = !line.contains("*/");
            continue;
        }
        let body = strip_vis(line);
        if let Some(rest) = body.strip_prefix("mod ") {
            if rest.contains('{') {
                break; // inline module body: the import head is over
            }
            if let Some(p) = resolve_rust_mod(file, rest.trim_end_matches(';').trim()) {
                out.push(p);
            }
            continue;
        }
        if let Some(rest) = body.strip_prefix("use ") {
            let mut stmt = rest.to_string();
            let mut joined = 0;
            while !stmt.contains(';') && i < head.len() && joined < JOIN_MAX_LINES {
                stmt.push(' ');
                stmt.push_str(head[i].trim());
                i += 1;
                joined += 1;
            }
            let stmt = stmt.split(';').next().unwrap_or("").trim().to_string();
            for segs in expand_use_paths(&stmt) {
                if let Some(p) = resolve_use_crate(file, root, &segs) {
                    out.push(p);
                }
            }
            continue;
        }
        if body.starts_with("extern crate") {
            continue;
        }
        break; // first non-import item ends the head
    }
    out
}

/// Expands `crate::a::{b, c::d}` into segment lists (one brace level; nested
/// groups are skipped — unresolvable means no edge, never a guessed one).
/// Non-`crate` paths (std, external crates) are dropped: they cannot be
/// project files.
fn expand_use_paths(stmt: &str) -> Vec<Vec<String>> {
    let (prefix, items): (&str, Vec<&str>) = match stmt.find('{') {
        Some(open) => {
            let Some(close) = stmt.rfind('}') else { return Vec::new() };
            let inner = &stmt[open + 1..close];
            if inner.contains('{') {
                return Vec::new();
            }
            (stmt[..open].trim_end_matches("::").trim(), inner.split(',').map(str::trim).collect())
        }
        None => ("", vec![stmt.trim()]),
    };
    let mut out = Vec::new();
    for item in items {
        if item.is_empty() {
            continue;
        }
        let full = if prefix.is_empty() {
            item.to_string()
        } else if item == "self" {
            prefix.to_string()
        } else {
            format!("{prefix}::{item}")
        };
        let segs: Vec<String> = full
            .split("::")
            .map(|s| s.split_whitespace().next().unwrap_or("").to_string()) // drops " as alias"
            .filter(|s| !s.is_empty() && s != "*")
            .collect();
        if segs.first().map(String::as_str) == Some("crate") && segs.len() > 1 {
            out.push(segs[1..].to_vec());
        }
    }
    out
}

/// `mod foo;` resolves next to the declaring file (or under its directory for
/// non-root modules), per the standard layout. Path attributes are ignored:
/// unresolvable → no edge.
fn resolve_rust_mod(file: &Path, name: &str) -> Option<PathBuf> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let parent = file.parent()?;
    let stem = file.file_stem()?.to_str()?;
    let dir = if matches!(stem, "main" | "lib" | "mod") { parent.to_path_buf() } else { parent.join(stem) };
    first_file([dir.join(format!("{name}.rs")), dir.join(name).join("mod.rs")])
}

/// Nearest ancestor directory holding `lib.rs`/`main.rs` — the crate source
/// root that `use crate::…` paths are relative to.
fn crate_src_root(file: &Path, root: &Path) -> Option<PathBuf> {
    let mut dir = file.parent()?;
    loop {
        if !dir.starts_with(root) {
            return None;
        }
        if dir.join("lib.rs").is_file() || dir.join("main.rs").is_file() {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

/// Resolves `use crate::a::b::c` longest-prefix-first, so a trailing symbol
/// name falls back to its containing module file.
fn resolve_use_crate(file: &Path, root: &Path, segs: &[String]) -> Option<PathBuf> {
    let src_root = crate_src_root(file, root)?;
    for take in (1..=segs.len()).rev() {
        let mut base = src_root.clone();
        for s in &segs[..take] {
            base.push(s);
        }
        if let Some(p) = first_file([base.with_extension("rs"), base.join("mod.rs")]) {
            return Some(p);
        }
    }
    None
}

// ---------------------------------------------------------------- TS / JS

const TS_EXTS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

fn tsjs_edges(file: &Path, head: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut in_block_comment = false;
    while i < head.len() {
        let line = head[i].trim();
        i += 1;
        if in_block_comment {
            if line.contains("*/") {
                in_block_comment = false;
            }
            continue;
        }
        if line.is_empty() || line.starts_with("//") || line.starts_with("#!") {
            continue;
        }
        if line.starts_with("/*") {
            in_block_comment = !line.contains("*/");
            continue;
        }
        if line.trim_end_matches(';') == "'use strict'" || line.trim_end_matches(';') == "\"use strict\"" {
            continue;
        }
        let is_import = line.starts_with("import");
        let is_reexport = line.starts_with("export {")
            || line.starts_with("export *")
            || (line.starts_with("export") && line.contains(" from "));
        if is_import || is_reexport {
            // The specifier string may sit lines below (`import {\n a,\n} from 'x'`).
            let mut stmt = line.to_string();
            let mut joined = 0;
            while quoted(&stmt).is_none() && i < head.len() && joined < JOIN_MAX_LINES {
                stmt.push(' ');
                stmt.push_str(head[i].trim());
                i += 1;
                joined += 1;
            }
            if let Some(spec) = quoted(&stmt)
                && let Some(p) = resolve_tsjs(file, &spec)
            {
                out.push(p);
            }
            continue;
        }
        if let Some(pos) = line.find("require(") {
            if let Some(spec) = quoted(&line[pos..])
                && let Some(p) = resolve_tsjs(file, &spec)
            {
                out.push(p);
            }
            continue;
        }
        break;
    }
    out
}

/// Node/TS relative-specifier resolution: literal file, `.js`→`.ts` swap
/// (TS ESM imports name the compiled file), extension append, then
/// `index.*`. Bare package specifiers are never edges.
fn resolve_tsjs(file: &Path, spec: &str) -> Option<PathBuf> {
    if !(spec.starts_with("./") || spec.starts_with("../")) {
        return None;
    }
    let base = normalize(&file.parent()?.join(spec));
    if base.is_file() && edge_lang(&base).is_some() {
        return Some(base);
    }
    if let Some(ext) = base.extension().and_then(|e| e.to_str()) {
        let swapped: &[&str] = match ext {
            "js" => &["ts", "tsx"],
            "mjs" => &["mts"],
            "cjs" => &["cts"],
            "jsx" => &["tsx"],
            _ => &[],
        };
        for e in swapped {
            let c = base.with_extension(e);
            if c.is_file() {
                return Some(c);
            }
        }
    }
    for e in TS_EXTS {
        let c = PathBuf::from(format!("{}.{e}", base.display()));
        if c.is_file() {
            return Some(c);
        }
    }
    for e in TS_EXTS {
        let c = base.join(format!("index.{e}"));
        if c.is_file() {
            return Some(c);
        }
    }
    None
}

// ---------------------------------------------------------------- Python

fn join_dotted(base: &Path, dotted: &str) -> PathBuf {
    let mut p = base.to_path_buf();
    for seg in dotted.split('.').filter(|s| !s.is_empty()) {
        p.push(seg);
    }
    p
}

/// `import a.b` resolved against the file's own directory: stdlib and
/// site-packages imports fail the existence check and produce no edge.
fn resolve_py_module(dir: &Path, module: &str) -> Option<PathBuf> {
    if module.is_empty() || module.starts_with('.') {
        return None;
    }
    let p = join_dotted(dir, module);
    first_file([p.with_extension("py"), p.join("__init__.py")])
}

fn python_edges(file: &Path, head: &[&str]) -> Vec<PathBuf> {
    let Some(dir) = file.parent().map(Path::to_path_buf) else { return Vec::new() };
    let mut out = Vec::new();
    let mut i = 0;
    let mut in_docstring: Option<&str> = None;
    while i < head.len() {
        let line = head[i].trim();
        i += 1;
        if let Some(q) = in_docstring {
            if line.contains(q) {
                in_docstring = None;
            }
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(q) = ["\"\"\"", "'''"].into_iter().find(|q| line.starts_with(q)) {
            if !line[3..].contains(q) {
                in_docstring = Some(q);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("import ") {
            for part in rest.split(',') {
                if let Some(module) = part.split_whitespace().next()
                    && let Some(p) = resolve_py_module(&dir, module)
                {
                    out.push(p);
                }
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("from ") {
            let mut stmt = rest.to_string();
            let mut joined = 0;
            while stmt.contains('(') && !stmt.contains(')') && i < head.len() && joined < JOIN_MAX_LINES {
                stmt.push(' ');
                stmt.push_str(head[i].trim());
                i += 1;
                joined += 1;
            }
            python_from_edges(&dir, &stmt, &mut out);
            continue;
        }
        break;
    }
    out
}

/// `from <module> import <names>`: each name is tried as a submodule file
/// first (`from .pkg import mod` is a real module edge); names that do not
/// resolve fall back to one edge on the module itself — that is where the
/// symbol lives.
fn python_from_edges(dir: &Path, stmt: &str, out: &mut Vec<PathBuf>) {
    let Some((module_part, names_part)) = stmt.split_once(" import ") else { return };
    let module_part = module_part.trim();
    let dots = module_part.chars().take_while(|c| *c == '.').count();
    let rel = &module_part[dots..];
    let mut base = dir.to_path_buf();
    for _ in 1..dots {
        let Some(parent) = base.parent() else { return };
        base = parent.to_path_buf();
    }
    let module_dir = if rel.is_empty() { base.clone() } else { join_dotted(&base, rel) };
    let mut any_unmatched = false;
    for name in names_part.split(',') {
        let name = name.trim().trim_start_matches('(').trim_end_matches(')').trim();
        let Some(name) = name.split_whitespace().next() else { continue };
        if name == "*" {
            any_unmatched = true;
            continue;
        }
        let target = module_dir.join(name);
        match first_file([target.with_extension("py"), target.join("__init__.py")]) {
            Some(p) => out.push(p),
            None => any_unmatched = true,
        }
    }
    if any_unmatched {
        let fallback = if rel.is_empty() {
            first_file([base.join("__init__.py")])
        } else {
            let m = join_dotted(&base, rel);
            first_file([m.with_extension("py"), m.join("__init__.py")])
        };
        if let Some(p) = fallback {
            out.push(p);
        }
    }
}

// ---------------------------------------------------------------- Go

/// Nearest `go.mod` above the file (inside the root) and its module path.
fn go_module(file: &Path, root: &Path) -> Option<(PathBuf, String)> {
    let mut dir = file.parent()?;
    loop {
        if !dir.starts_with(root) {
            return None;
        }
        let gm = dir.join("go.mod");
        if gm.is_file() {
            let text = std::fs::read_to_string(&gm).ok()?;
            let module = text.lines().find_map(|l| l.trim().strip_prefix("module "))?.trim().to_string();
            if module.is_empty() {
                return None;
            }
            return Some((dir.to_path_buf(), module));
        }
        dir = dir.parent()?;
    }
}

/// A Go import names a package (directory); the edge targets are that
/// package's non-test `.go` files.
fn go_pkg_edges(mod_dir: &Path, mod_path: &str, spec: &str, out: &mut Vec<PathBuf>) {
    let sub = if spec == mod_path {
        String::new()
    } else {
        match spec.strip_prefix(&format!("{mod_path}/")) {
            Some(s) => s.to_string(),
            None => return, // external package: no edge
        }
    };
    let dir = if sub.is_empty() { mod_dir.to_path_buf() } else { mod_dir.join(sub) };
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let is_go = p.extension().and_then(|x| x.to_str()) == Some("go");
        let is_test = p
            .file_name()
            .and_then(|n| n.to_str())
            .is_none_or(|n| n.ends_with("_test.go"));
        if is_go && !is_test && p.is_file() {
            out.push(p);
        }
    }
}

fn go_edges(file: &Path, head: &[&str], root: &Path) -> Vec<PathBuf> {
    let Some((mod_dir, mod_path)) = go_module(file, root) else { return Vec::new() };
    let mut out = Vec::new();
    let mut i = 0;
    let mut in_block_comment = false;
    let mut in_import_block = false;
    while i < head.len() {
        let line = head[i].trim();
        i += 1;
        if in_block_comment {
            if line.contains("*/") {
                in_block_comment = false;
            }
            continue;
        }
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if line.starts_with("/*") {
            in_block_comment = !line.contains("*/");
            continue;
        }
        if in_import_block {
            if line.starts_with(')') {
                in_import_block = false;
            } else if let Some(spec) = quoted(line) {
                go_pkg_edges(&mod_dir, &mod_path, &spec, &mut out);
            }
            continue;
        }
        if line.starts_with("package ") {
            continue;
        }
        if line.starts_with("import (") {
            in_import_block = true;
            continue;
        }
        if line.starts_with("import ") {
            if let Some(spec) = quoted(line) {
                go_pkg_edges(&mod_dir, &mod_path, &spec, &mut out);
            }
            continue;
        }
        break;
    }
    out
}

// ------------------------------------------------------------ persistence

pub fn ensure_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS import_edges(
           src TEXT NOT NULL,
           dst TEXT NOT NULL,
           PRIMARY KEY(src, dst)
         );
         CREATE INDEX IF NOT EXISTS import_edges_dst ON import_edges(dst);",
    )?;
    Ok(())
}

/// Replaces the outgoing edges of one file. Keyed by path, not rowid:
/// `code_files` rows are re-inserted on change and rowids must not carry
/// meaning across scans.
pub fn replace_file_edges(conn: &Connection, src: &str, dsts: &[PathBuf]) -> anyhow::Result<()> {
    conn.execute("DELETE FROM import_edges WHERE src=?1", [src])?;
    for d in dsts {
        conn.execute(
            "INSERT OR IGNORE INTO import_edges(src, dst) VALUES(?1, ?2)",
            rusqlite::params![src, d.to_string_lossy()],
        )?;
    }
    Ok(())
}

/// Drops edges whose endpoints left the index (deleted or newly ignored
/// files) so the ranking never references ghosts.
pub fn prune_edges(conn: &Connection) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM import_edges
         WHERE src NOT IN (SELECT path FROM code_files)
            OR dst NOT IN (SELECT path FROM code_files)",
        [],
    )?;
    Ok(())
}

// -------------------------------------------------------------- pagerank

/// PageRank with the module-doc parameters. Dangling mass is redistributed
/// through the restart vector, which doubles as the personalization hook: a
/// `None` restart means uniform (classic PageRank). The restart vector need
/// not be normalized; an all-zero one degrades to uniform.
pub fn pagerank(n: usize, edges: &[(usize, usize)], restart: Option<&[f64]>) -> Vec<f64> {
    if n == 0 {
        return Vec::new();
    }
    let uniform = 1.0 / n as f64;
    let restart: Vec<f64> = match restart {
        Some(r) if r.len() == n => {
            let sum: f64 = r.iter().sum();
            if sum > 0.0 { r.iter().map(|v| v / sum).collect() } else { vec![uniform; n] }
        }
        _ => vec![uniform; n],
    };
    let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(s, d) in edges {
        if s < n && d < n && s != d {
            adjacency[s].push(d);
        }
    }
    let mut p = restart.clone();
    for _ in 0..ITERATIONS {
        let mut next = vec![0.0; n];
        let mut dangling = 0.0;
        for (i, targets) in adjacency.iter().enumerate() {
            if targets.is_empty() {
                dangling += p[i];
            } else {
                let share = p[i] / targets.len() as f64;
                for &d in targets {
                    next[d] += share;
                }
            }
        }
        for (nx, r) in next.iter_mut().zip(&restart) {
            *nx = (1.0 - DAMPING) * r + DAMPING * (*nx + dangling * r);
        }
        p = next;
    }
    p
}

/// Rank percentiles (0–100, Hazen definition), ties averaged: nodes with
/// identical PageRank — every isolated file, for instance — must show the
/// identical percentile, not an accident of sort order. Exact float equality
/// is correct here: tied nodes go through identical arithmetic.
pub fn percentiles(ranks: &[f64]) -> Vec<f64> {
    let n = ranks.len();
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| ranks[a].total_cmp(&ranks[b]));
    let mut out = vec![0.0; n];
    let mut i = 0;
    while i < n {
        let mut j = i;
        while j + 1 < n && ranks[idx[j + 1]] == ranks[idx[i]] {
            j += 1;
        }
        let avg_pos = (i + j + 2) as f64 / 2.0; // 1-based positions i+1 ..= j+1
        let pct = (avg_pos - 0.5) / n as f64 * 100.0;
        for &k in &idx[i..=j] {
            out[k] = pct;
        }
        i = j + 1;
    }
    out
}

fn files_under(conn: &Connection, root: &Path) -> anyhow::Result<Vec<(String, u64)>> {
    let mut stmt = conn.prepare("SELECT path, size FROM code_files ORDER BY path")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?;
    Ok(rows.filter_map(Result::ok).filter(|(p, _)| Path::new(p).starts_with(root)).collect())
}

fn edges_among(conn: &Connection, index: &BTreeMap<&str, usize>) -> anyhow::Result<Vec<(usize, usize)>> {
    let mut stmt = conn.prepare("SELECT src, dst FROM import_edges")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    Ok(rows
        .filter_map(Result::ok)
        .filter_map(|(s, d)| Some((*index.get(s.as_str())?, *index.get(d.as_str())?)))
        .collect())
}

/// Recomputes stored rank percentiles, one graph per code root (imports
/// never resolve across roots). A root with zero edges gets NULL scores:
/// ranking files on no evidence would order them by accident.
pub fn recompute_ranks(conn: &Connection, roots: &[PathBuf]) -> anyhow::Result<()> {
    ensure_schema(conn)?;
    for root in roots {
        let files = files_under(conn, root)?;
        if files.is_empty() {
            continue;
        }
        let index: BTreeMap<&str, usize> =
            files.iter().enumerate().map(|(i, (p, _))| (p.as_str(), i)).collect();
        let edges = edges_among(conn, &index)?;
        if edges.is_empty() {
            for (f, _) in &files {
                conn.execute("UPDATE code_files SET rank_pct=NULL WHERE path=?1", [f])?;
            }
            continue;
        }
        let ranks = pagerank(files.len(), &edges, None);
        let pcts = percentiles(&ranks);
        for ((f, _), pct) in files.iter().zip(&pcts) {
            conn.execute("UPDATE code_files SET rank_pct=?1 WHERE path=?2", rusqlite::params![pct, f])?;
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ map

pub struct RepoMap {
    pub lines: Vec<String>,
    pub total_files: usize,
    pub focus_matched: bool,
}

fn build_restart(
    focus: Option<&str>,
    paths: &[(String, String, u64)],
    symbols: &[Vec<String>],
) -> (Option<Vec<f64>>, bool) {
    let Some(term) = focus else { return (None, false) };
    let q = crate::code::norm_ident(term);
    if q.is_empty() {
        return (None, false);
    }
    let seeds: Vec<f64> = paths
        .iter()
        .zip(symbols)
        .map(|((_, rel, _), syms)| {
            let hit = crate::code::norm_ident(rel).contains(&q)
                || syms.iter().any(|s| crate::code::norm_ident(s).contains(&q));
            if hit { 1.0 } else { 0.0 }
        })
        .collect();
    if seeds.iter().sum::<f64>() == 0.0 { (None, false) } else { (Some(seeds), true) }
}

/// The repo map: every indexed file under `roots`, ordered by (personalized)
/// PageRank, rendered one line per file and fitted to `budget_tokens` by
/// binary search over the entry count (rendered size is monotone in the
/// count, so the largest fitting count is found in O(log n) probes). At
/// least one entry survives any budget — an empty map answers nothing.
pub fn map(
    conn: &Connection,
    roots: &[PathBuf],
    focus: Option<&str>,
    budget_tokens: u64,
) -> anyhow::Result<RepoMap> {
    crate::code::ensure_schema(conn)?;
    let mut paths: Vec<(String, String, u64)> = Vec::new(); // (abs, display-relative, size)
    for root in roots {
        for (p, size) in files_under(conn, root)? {
            let rel = Path::new(&p)
                .strip_prefix(root)
                .map(|r| r.display().to_string())
                .unwrap_or_else(|_| p.clone());
            paths.push((p, rel, size));
        }
    }
    paths.sort();
    paths.dedup_by(|a, b| a.0 == b.0);
    let n = paths.len();
    if n == 0 {
        return Ok(RepoMap { lines: Vec::new(), total_files: 0, focus_matched: false });
    }
    let index: BTreeMap<&str, usize> =
        paths.iter().enumerate().map(|(i, (p, _, _))| (p.as_str(), i)).collect();
    let edges = edges_among(conn, &index)?;
    let mut symbols: Vec<Vec<String>> = vec![Vec::new(); n];
    {
        let mut stmt = conn.prepare(
            "SELECT f.path, s.name FROM symbols s JOIN code_files f ON f.id = s.file_id
             ORDER BY f.path, s.start_line",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for (p, name) in rows.filter_map(Result::ok) {
            if let Some(&i) = index.get(p.as_str()) {
                symbols[i].push(name);
            }
        }
    }
    let (restart, focus_matched) = build_restart(focus, &paths, &symbols);
    let ranks = pagerank(n, &edges, restart.as_deref());
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| ranks[b].total_cmp(&ranks[a]).then(paths[a].1.cmp(&paths[b].1)));
    let lines: Vec<String> = order
        .iter()
        .map(|&i| {
            let (_, rel, size) = &paths[i];
            let tok = crate::hook_io::estimate_tokens(*size as usize);
            let syms = &symbols[i];
            if syms.is_empty() {
                format!("{rel} (~{tok} tok)")
            } else {
                let shown: Vec<&str> = syms.iter().take(MAP_SYMBOLS_SHOWN).map(String::as_str).collect();
                let more = if syms.len() > MAP_SYMBOLS_SHOWN { ", …" } else { "" };
                format!("{rel} ({}{more}, ~{tok} tok)", shown.join(", "))
            }
        })
        .collect();
    let cost = |k: usize| -> u64 {
        let chars: usize = lines[..k].iter().map(|l| l.len() + 1).sum();
        crate::hook_io::estimate_tokens(chars)
    };
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if cost(mid) <= budget_tokens {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let keep = lo.max(1).min(n);
    Ok(RepoMap { lines: lines[..keep].to_vec(), total_files: n, focus_matched })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, content: &str) -> PathBuf {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, content).unwrap();
        p
    }

    // ---- edge extraction: Rust

    #[test]
    fn rust_mod_and_use_crate_edges() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let main = write(
            root,
            "src/main.rs",
            "mod alpha;\npub mod beta;\nuse crate::gamma::thing;\n\nfn main() {}\n",
        );
        let alpha = write(root, "src/alpha.rs", "pub fn a() {}\n");
        let beta = write(root, "src/beta/mod.rs", "pub fn b() {}\n");
        let gamma = write(root, "src/gamma.rs", "pub fn thing() {}\n");
        let src = std::fs::read_to_string(&main).unwrap();
        let edges = extract_edges(&main, &src, root);
        assert_eq!(edges, vec![alpha, beta, gamma]);
    }

    #[test]
    fn rust_submodule_and_group_use() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/lib.rs", "mod a;\nmod b;\nmod c;\n");
        let a = write(root, "src/a.rs", "mod sub;\nuse crate::{b, c::helper as h};\n");
        let sub = write(root, "src/a/sub.rs", "");
        let b = write(root, "src/b.rs", "");
        let c = write(root, "src/c.rs", "pub fn helper() {}\n");
        let src = std::fs::read_to_string(&a).unwrap();
        let edges = extract_edges(&a, &src, root);
        assert_eq!(edges, vec![sub, b, c]);
    }

    #[test]
    fn rust_unresolvable_and_external_imports_make_no_edges() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let main = write(
            root,
            "src/main.rs",
            "use std::fmt;\nuse anyhow::Result;\nmod ghost;\nuse crate::missing::x;\n\nfn main() {}\n",
        );
        let src = std::fs::read_to_string(&main).unwrap();
        assert!(extract_edges(&main, &src, root).is_empty(), "unresolvable must never invent edges");
    }

    #[test]
    fn rust_imports_after_the_head_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let main = write(root, "src/main.rs", "fn main() {}\nuse crate::late;\n");
        write(root, "src/late.rs", "");
        let src = std::fs::read_to_string(&main).unwrap();
        assert!(extract_edges(&main, &src, root).is_empty(), "head-of-file only");
    }

    #[test]
    fn rust_multiline_group_use_is_joined() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/main.rs", "");
        let a = write(root, "src/a.rs", "use crate::{\n    b,\n    c,\n};\n");
        let b = write(root, "src/b.rs", "");
        let c = write(root, "src/c.rs", "");
        let src = std::fs::read_to_string(&a).unwrap();
        assert_eq!(extract_edges(&a, &src, root), vec![b, c]);
    }

    // ---- edge extraction: TS/JS

    #[test]
    fn ts_relative_imports_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let app = write(
            root,
            "src/app.ts",
            "import { x } from './util';\nimport def from '../lib/helper.js';\nimport './side';\nconst legacy = require('./legacy');\nimport pkg from 'react';\n",
        );
        let util = write(root, "src/util.ts", "");
        let helper = write(root, "lib/helper.ts", "");
        let side = write(root, "src/side.ts", "");
        let legacy = write(root, "src/legacy.js", "");
        let src = std::fs::read_to_string(&app).unwrap();
        let edges = extract_edges(&app, &src, root);
        assert_eq!(edges, vec![helper, legacy, side, util], "sorted: bare 'react' makes no edge");
    }

    #[test]
    fn ts_multiline_index_and_reexport_imports() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let app = write(
            root,
            "src/app.ts",
            "import {\n  a,\n  b,\n} from './widgets';\nexport { c } from './re';\n",
        );
        let widgets = write(root, "src/widgets/index.ts", "");
        let re = write(root, "src/re.tsx", "");
        let src = std::fs::read_to_string(&app).unwrap();
        assert_eq!(extract_edges(&app, &src, root), vec![re, widgets]);
    }

    #[test]
    fn ts_edges_stay_inside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root_dir = dir.path().join("root");
        std::fs::create_dir_all(&root_dir).unwrap();
        write(dir.path(), "escape.ts", "");
        let a = write(&root_dir, "a.ts", "import { e } from '../escape';\n");
        let src = std::fs::read_to_string(&a).unwrap();
        assert!(extract_edges(&a, &src, &root_dir).is_empty(), "an edge may never leave the code root");
    }

    // ---- edge extraction: Python

    #[test]
    fn python_relative_imports_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let m = write(
            root,
            "pkg/mod.py",
            "\"\"\"Docstring\nspanning lines.\n\"\"\"\nfrom . import sibling\nfrom .helpers import util\nfrom ..outer import thing\nimport local_module\nimport os\n",
        );
        let sibling = write(root, "pkg/sibling.py", "");
        let helpers = write(root, "pkg/helpers.py", "def util(): pass\n");
        let outer = write(root, "outer.py", "def thing(): pass\n");
        let local = write(root, "pkg/local_module.py", "");
        let src = std::fs::read_to_string(&m).unwrap();
        let edges = extract_edges(&m, &src, root);
        assert_eq!(edges, vec![outer, helpers, local, sibling], "sorted; os makes no edge");
    }

    #[test]
    fn python_submodule_import_beats_package_init() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let m = write(root, "pkg/mod.py", "from .sub import child\n");
        write(root, "pkg/sub/__init__.py", "");
        let child = write(root, "pkg/sub/child.py", "");
        let src = std::fs::read_to_string(&m).unwrap();
        assert_eq!(extract_edges(&m, &src, root), vec![child], "resolved names must not add the __init__ fallback");
    }

    #[test]
    fn python_stdlib_imports_make_no_edges() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let m = write(root, "pkg/mod.py", "import os, sys\nfrom collections import OrderedDict\n");
        let src = std::fs::read_to_string(&m).unwrap();
        assert!(extract_edges(&m, &src, root).is_empty());
    }

    // ---- edge extraction: Go

    #[test]
    fn go_internal_package_imports_resolve_to_package_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "go.mod", "module example.com/proj\n\ngo 1.22\n");
        let main = write(
            root,
            "cmd/main.go",
            "package main\n\nimport (\n\t\"fmt\"\n\t\"example.com/proj/internal/util\"\n)\n\nfunc main() {}\n",
        );
        let util = write(root, "internal/util/util.go", "package util\n");
        let extra = write(root, "internal/util/extra.go", "package util\n");
        write(root, "internal/util/util_test.go", "package util\n");
        let src = std::fs::read_to_string(&main).unwrap();
        assert_eq!(extract_edges(&main, &src, root), vec![extra, util], "fmt and _test.go excluded");
    }

    #[test]
    fn go_without_gomod_makes_no_edges() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let main = write(root, "main.go", "package main\n\nimport \"example.com/x/y\"\n");
        let src = std::fs::read_to_string(&main).unwrap();
        assert!(extract_edges(&main, &src, root).is_empty());
    }

    // ---- pagerank

    #[test]
    fn pagerank_cycle_is_uniform() {
        let p = pagerank(3, &[(0, 1), (1, 2), (2, 0)], None);
        for v in &p {
            assert!((v - 1.0 / 3.0).abs() < 1e-6, "symmetric cycle must rank uniformly: {p:?}");
        }
    }

    #[test]
    fn pagerank_dangling_mass_is_redistributed() {
        // Two nodes, 0 -> 1, node 1 dangling. Fixed point of
        //   a = 0.15/2 + 0.85*b/2,  b = 0.15/2 + 0.85*(a + b/2)
        // is a = 0.350877, b = 0.649123.
        let p = pagerank(2, &[(0, 1)], None);
        assert!((p[0] - 0.350877).abs() < 5e-3, "{p:?}");
        assert!((p[1] - 0.649123).abs() < 5e-3, "{p:?}");
        assert!((p[0] + p[1] - 1.0).abs() < 1e-9, "dangling mass must not leak: {p:?}");
    }

    #[test]
    fn pagerank_personalized_restart_dominates_without_edges() {
        let p = pagerank(3, &[], Some(&[0.0, 1.0, 0.0]));
        assert!(p[1] > 0.999, "{p:?}");
        assert!(p[0] < 1e-9 && p[2] < 1e-9, "{p:?}");
    }

    #[test]
    fn pagerank_star_ranks_the_hub_highest() {
        let p = pagerank(4, &[(1, 0), (2, 0), (3, 0)], None);
        assert!(p[0] > p[1] && p[0] > p[2] && p[0] > p[3], "{p:?}");
    }

    // ---- percentiles

    #[test]
    fn percentiles_are_hazen_with_average_rank_ties() {
        let distinct = percentiles(&[0.1, 0.3, 0.2]);
        assert!((distinct[0] - 16.666).abs() < 1e-2, "{distinct:?}");
        assert!((distinct[1] - 83.333).abs() < 1e-2, "{distinct:?}");
        assert!((distinct[2] - 50.0).abs() < 1e-9, "{distinct:?}");

        let tied = percentiles(&[0.2, 0.1, 0.2]);
        assert!((tied[0] - 66.666).abs() < 1e-2, "{tied:?}");
        assert!((tied[1] - 16.666).abs() < 1e-2, "{tied:?}");
        assert_eq!(tied[0], tied[2], "equal ranks must get the identical percentile");
    }

    // ---- persisted ranks through scan_code

    fn rust_project(root: &Path) {
        write(root, "main.rs", "mod a;\nmod b;\nfn main() {}\n");
        write(root, "a.rs", "use crate::b::xb;\npub fn xa() {}\n");
        write(root, "b.rs", "pub fn xb() {}\n");
    }

    fn rank_of(conn: &Connection, path: &Path) -> Option<f64> {
        conn.query_row(
            "SELECT rank_pct FROM code_files WHERE path=?1",
            [path.to_string_lossy()],
            |r| r.get::<_, Option<f64>>(0),
        )
        .unwrap()
    }

    #[test]
    fn scan_persists_edges_and_percentiles() {
        let dir = tempfile::tempdir().unwrap();
        rust_project(dir.path());
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        let report = crate::code::scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();
        assert_eq!(report.edges, 3, "main->a, main->b, a->b");
        let b = rank_of(&conn, &dir.path().join("b.rs")).unwrap();
        let a = rank_of(&conn, &dir.path().join("a.rs")).unwrap();
        let m = rank_of(&conn, &dir.path().join("main.rs")).unwrap();
        assert!(b > a && a > m, "importance must follow in-links: b={b} a={a} main={m}");
    }

    #[test]
    fn zero_edge_project_gets_no_scores() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "one.rs", "pub fn one() {}\n");
        write(dir.path(), "two.rs", "pub fn two() {}\n");
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        let report = crate::code::scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();
        assert_eq!(report.edges, 0);
        assert!(rank_of(&conn, &dir.path().join("one.rs")).is_none(), "no signal beats a false one");
        assert!(rank_of(&conn, &dir.path().join("two.rs")).is_none());
        let hits = crate::code::find(&conn, "one", 10).unwrap();
        assert!(hits[0].rank_pct.is_none());
    }

    #[test]
    fn rescan_updates_edges_when_imports_change() {
        let dir = tempfile::tempdir().unwrap();
        rust_project(dir.path());
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        crate::code::scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();
        // Drop a.rs's import (different size, so the change is seen even
        // within one mtime second).
        std::fs::write(dir.path().join("a.rs"), "pub fn xa_standalone() {}\n").unwrap();
        let report = crate::code::scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();
        assert_eq!(report.edges, 2, "a->b must be gone");
        // Deleting b.rs prunes its remaining edge.
        std::fs::remove_file(dir.path().join("b.rs")).unwrap();
        let report = crate::code::scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();
        assert_eq!(report.edges, 1, "main->a survives, main->b pruned");
    }

    // ---- map

    #[test]
    fn map_orders_by_rank_and_fits_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        rust_project(dir.path());
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        crate::code::scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();
        let roots = vec![dir.path().to_path_buf()];

        let all = map(&conn, &roots, None, 100_000).unwrap();
        assert_eq!(all.total_files, 3);
        assert_eq!(all.lines.len(), 3);
        assert!(all.lines[0].starts_with("b.rs"), "most-imported file first: {:?}", all.lines);
        assert!(all.lines[0].contains("xb"), "top symbols shown: {:?}", all.lines[0]);

        // Fitting is exact: a budget of cost(first two lines) keeps two,
        // one token less keeps one.
        let cost2 = crate::hook_io::estimate_tokens(all.lines[0].len() + 1 + all.lines[1].len() + 1);
        assert_eq!(map(&conn, &roots, None, cost2).unwrap().lines.len(), 2);
        assert_eq!(map(&conn, &roots, None, cost2 - 1).unwrap().lines.len(), 1);

        let starved = map(&conn, &roots, None, 1).unwrap();
        assert_eq!(starved.lines.len(), 1, "at least one entry under any budget");
    }

    #[test]
    fn map_focus_personalizes_the_ordering() {
        let dir = tempfile::tempdir().unwrap();
        rust_project(dir.path());
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        crate::code::scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();
        let roots = vec![dir.path().to_path_buf()];

        let focused = map(&conn, &roots, Some("xa"), 100_000).unwrap();
        assert!(focused.focus_matched);
        assert!(focused.lines[0].starts_with("a.rs"), "focus seed must lead: {:?}", focused.lines);

        let miss = map(&conn, &roots, Some("zzz_nothing"), 100_000).unwrap();
        assert!(!miss.focus_matched, "an unmatched focus degrades to the plain map");
        assert!(miss.lines[0].starts_with("b.rs"));
    }
}
