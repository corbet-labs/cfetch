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

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Component, Path, PathBuf};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ImportBinding {
    pub local_name: String,
    pub target_name: Option<String>,
    pub namespace: bool,
    pub type_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportFact {
    pub target: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
    pub bindings: Vec<ImportBinding>,
}

impl ImportFact {
    fn new(
        target: PathBuf,
        start_line: usize,
        end_line: usize,
        bindings: Vec<ImportBinding>,
    ) -> Self {
        Self { target, start_line, end_line, bindings }
    }
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

/// Compatibility view used by extraction tests and callers that need only
/// topology. Exact source ranges and bindings live in [`extract_imports`].
#[cfg(test)]
fn extract_edges(file: &Path, source: &str, root: &Path) -> Vec<PathBuf> {
    let paths: BTreeSet<PathBuf> =
        extract_imports(file, source, root).into_iter().map(|fact| fact.target).collect();
    paths.into_iter().collect()
}

/// Extracts resolvable project-internal import facts. Every fact retains the
/// exact source statement range and any explicit local binding that can later
/// resolve a parser-observed symbol use without guessing by name alone.
pub(crate) fn extract_imports(file: &Path, source: &str, root: &Path) -> Vec<ImportFact> {
    let Some(lang) = edge_lang(file) else { return Vec::new() };
    let head: Vec<&str> = source.lines().take(HEAD_MAX_LINES).collect();
    let raw = match lang {
        EdgeLang::Rust => rust_edges(file, &head, root),
        EdgeLang::TsJs => tsjs_edges(file, &head),
        EdgeLang::Python => python_edges(file, &head),
        EdgeLang::Go => go_edges(file, &head, root),
    };
    let self_path = normalize(file);
    let mut out: BTreeMap<(PathBuf, usize, usize), Vec<ImportBinding>> = BTreeMap::new();
    for mut fact in raw {
        fact.target = normalize(&fact.target);
        if fact.target != self_path
            && fact.target.starts_with(root)
            && crate::code::is_indexable(&fact.target)
        {
            let bindings = out
                .entry((fact.target, fact.start_line, fact.end_line))
                .or_default();
            bindings.append(&mut fact.bindings);
        }
    }
    out.into_iter()
        .map(|((target, start_line, end_line), mut bindings)| {
            bindings.sort();
            bindings.dedup();
            ImportFact::new(target, start_line, end_line, bindings)
        })
        .collect()
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

fn rust_edges(file: &Path, head: &[&str], root: &Path) -> Vec<ImportFact> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut in_block_comment = false;
    while i < head.len() {
        let start_line = i + 1;
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
            let name = rest.trim_end_matches(';').trim();
            if let Some(p) = resolve_rust_mod(file, name) {
                out.push(ImportFact::new(
                    p,
                    start_line,
                    start_line,
                    vec![ImportBinding {
                        local_name: name.to_string(),
                        target_name: None,
                        namespace: true,
                        type_only: false,
                    }],
                ));
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
            for expanded in expand_use_paths(&stmt) {
                if let Some((p, resolved_segments)) =
                    resolve_use_crate(file, root, &expanded.segments)
                {
                    let namespace = resolved_segments == expanded.segments.len();
                    out.push(ImportFact::new(
                        p,
                        start_line,
                        i,
                        vec![ImportBinding {
                            local_name: expanded.local_name,
                            target_name: if namespace {
                                None
                            } else {
                                expanded.segments.last().cloned()
                            },
                            namespace,
                            type_only: false,
                        }],
                    ));
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

struct ExpandedUse {
    segments: Vec<String>,
    local_name: String,
}

/// Expands `crate::a::{b, c::d}` into segment lists (one brace level; nested
/// groups are skipped — unresolvable means no edge, never a guessed one).
/// Non-`crate` paths (std, external crates) are dropped: they cannot be
/// project files.
fn expand_use_paths(stmt: &str) -> Vec<ExpandedUse> {
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
        let (path, alias) = full
            .rsplit_once(" as ")
            .map_or((full.as_str(), None), |(path, alias)| (path.trim(), Some(alias.trim())));
        let mut segs: Vec<String> = path
            .split("::")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s != "*")
            .collect();
        if segs.first().map(String::as_str) == Some("crate") && segs.len() > 1 {
            let self_import = segs.last().map(String::as_str) == Some("self");
            if self_import {
                segs.pop();
            }
            let segments = segs[1..].to_vec();
            let local_name = alias
                .filter(|alias| !alias.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| segments.last().cloned().unwrap_or_default());
            out.push(ExpandedUse { segments, local_name });
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
/// root that `use crate::…` paths are relative to. A file under a `bin`
/// component is its OWN crate: `src/bin/tool.rs` and `src/bin/tool/main.rs`
/// are separate crates from the `src/lib.rs` beside them, so walking up to
/// the nearest lib/main would resolve `crate::util` against the LIBRARY's
/// modules — manufacturing a false edge to `src/util.rs` whenever the names
/// collide, and a missing edge whenever they do not.
fn crate_src_root(file: &Path, root: &Path) -> Option<PathBuf> {
    let parent = file.parent()?;
    let stem = file.file_stem()?.to_str()?;
    // Inside a `bin` (or examples/tests) tree: the file is a crate root of
    // its own — `tool.rs` resolves modules from `src/bin/tool/…`, and
    // `src/bin/tool/main.rs` from its own directory.
    let in_bin_tree = parent
        .ancestors()
        .take_while(|d| d.starts_with(root))
        .any(|d| matches!(d.file_name().and_then(|n| n.to_str()), Some("bin") | Some("examples") | Some("tests")));
    if in_bin_tree {
        if stem == "main" || stem == "lib" {
            return Some(parent.to_path_buf());
        }
        // `src/bin/tool.rs`: the crate root IS the file; modules resolve
        // relative to `src/bin/tool/` (created by resolve_rust_mod).
        return Some(file.to_path_buf());
    }
    let mut dir = parent;
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
fn resolve_use_crate(
    file: &Path,
    root: &Path,
    segs: &[String],
) -> Option<(PathBuf, usize)> {
    let src_root = crate_src_root(file, root)?;
    for take in (1..=segs.len()).rev() {
        let mut base = src_root.clone();
        for s in &segs[..take] {
            base.push(s);
        }
        if let Some(p) = first_file([base.with_extension("rs"), base.join("mod.rs")]) {
            return Some((p, take));
        }
    }
    None
}

// ---------------------------------------------------------------- TS / JS

const TS_EXTS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

fn tsjs_edges(file: &Path, head: &[&str]) -> Vec<ImportFact> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut in_block_comment = false;
    while i < head.len() {
        let start_line = i + 1;
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
                let bindings = if is_import {
                    ts_import_bindings(&stmt)
                } else {
                    Vec::new()
                };
                out.push(ImportFact::new(p, start_line, i, bindings));
            }
            continue;
        }
        if let Some(pos) = line.find("require(") {
            if let Some(spec) = quoted(&line[pos..])
                && let Some(p) = resolve_tsjs(file, &spec)
            {
                let binding = line[..pos]
                    .split('=')
                    .next_back()
                    .and_then(|left| left.split_whitespace().last())
                    .filter(|name| {
                        !name.is_empty()
                            && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$'))
                    })
                    .map(|name| ImportBinding {
                        local_name: name.to_string(),
                        target_name: None,
                        namespace: true,
                        type_only: false,
                    });
                out.push(ImportFact::new(
                    p,
                    start_line,
                    start_line,
                    binding.into_iter().collect(),
                ));
            }
            continue;
        }
        break;
    }
    out
}

fn ts_import_bindings(stmt: &str) -> Vec<ImportBinding> {
    let Some(before_from) = stmt.split(" from ").next() else { return Vec::new() };
    let mut clause = before_from.trim().strip_prefix("import").unwrap_or("").trim();
    let clause_type_only = clause.starts_with("type ");
    clause = clause.strip_prefix("type ").unwrap_or(clause).trim();
    if clause.is_empty() || clause.starts_with(['\'', '"']) {
        return Vec::new();
    }
    let mut bindings = Vec::new();
    if let Some(star) = clause.find("* as ")
        && let Some(name) = clause[star + 5..]
            .split([',', ' ', '\t'])
            .find(|part| !part.is_empty())
    {
        bindings.push(ImportBinding {
            local_name: name.to_string(),
            target_name: None,
            namespace: true,
            type_only: clause_type_only,
        });
    }
    if let Some(open) = clause.find('{')
        && let Some(close) = clause.rfind('}')
    {
        for item in clause[open + 1..close].split(',') {
            let item = item.trim();
            let item_type_only = clause_type_only || item.starts_with("type ");
            let item = item.strip_prefix("type ").unwrap_or(item);
            if item.is_empty() {
                continue;
            }
            let (target, local) = item
                .split_once(" as ")
                .map_or((item, item), |(target, local)| (target.trim(), local.trim()));
            if !target.is_empty() && !local.is_empty() {
                bindings.push(ImportBinding {
                    local_name: local.to_string(),
                    target_name: Some(target.to_string()),
                    namespace: false,
                    type_only: item_type_only,
                });
            }
        }
    }
    // A default import carries no source symbol name. Linking it to a
    // coincidentally same-named declaration would be a guess, so only named
    // and namespace imports become symbol bindings.
    bindings
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

fn python_edges(file: &Path, head: &[&str]) -> Vec<ImportFact> {
    let Some(dir) = file.parent().map(Path::to_path_buf) else { return Vec::new() };
    let mut out = Vec::new();
    let mut i = 0;
    let mut in_docstring: Option<&str> = None;
    while i < head.len() {
        let start_line = i + 1;
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
                let mut words = part.split_whitespace();
                if let Some(module) = words.next()
                    && let Some(p) = resolve_py_module(&dir, module)
                {
                    let alias = match (words.next(), words.next()) {
                        (Some("as"), Some(alias)) => Some(alias),
                        _ => None,
                    };
                    let local_name = alias.or_else(|| (!module.contains('.')).then_some(module));
                    out.push(ImportFact::new(
                        p,
                        start_line,
                        start_line,
                        local_name
                            .map(|local_name| ImportBinding {
                                local_name: local_name.to_string(),
                                target_name: None,
                                namespace: true,
                                type_only: false,
                            })
                            .into_iter()
                            .collect(),
                    ));
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
            python_from_edges(&dir, &stmt, start_line, i, &mut out);
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
fn python_from_edges(
    dir: &Path,
    stmt: &str,
    start_line: usize,
    end_line: usize,
    out: &mut Vec<ImportFact>,
) {
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
    let mut unmatched = Vec::new();
    let mut needs_fallback = false;
    for name in names_part.split(',') {
        let name = name.trim().trim_start_matches('(').trim_end_matches(')').trim();
        let mut words = name.split_whitespace();
        let Some(target_name) = words.next() else { continue };
        let local_name = match (words.next(), words.next()) {
            (Some("as"), Some(alias)) => alias,
            _ => target_name,
        };
        if target_name == "*" {
            needs_fallback = true;
            continue;
        }
        let target = module_dir.join(target_name);
        match first_file([target.with_extension("py"), target.join("__init__.py")]) {
            Some(p) => out.push(ImportFact::new(
                p,
                start_line,
                end_line,
                vec![ImportBinding {
                    local_name: local_name.to_string(),
                    target_name: None,
                    namespace: true,
                    type_only: false,
                }],
            )),
            None => {
                needs_fallback = true;
                unmatched.push(ImportBinding {
                    local_name: local_name.to_string(),
                    target_name: Some(target_name.to_string()),
                    namespace: false,
                    type_only: false,
                });
            }
        }
    }
    if needs_fallback {
        let fallback = if rel.is_empty() {
            first_file([base.join("__init__.py")])
        } else {
            let m = join_dotted(&base, rel);
            first_file([m.with_extension("py"), m.join("__init__.py")])
        };
        if let Some(p) = fallback {
            out.push(ImportFact::new(p, start_line, end_line, unmatched));
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
fn go_pkg_targets(mod_dir: &Path, mod_path: &str, spec: &str) -> Vec<PathBuf> {
    let sub = if spec == mod_path {
        String::new()
    } else {
        match spec.strip_prefix(&format!("{mod_path}/")) {
            Some(s) => s.to_string(),
            None => return Vec::new(), // external package: no edge
        }
    };
    let dir = if sub.is_empty() { mod_dir.to_path_buf() } else { mod_dir.join(sub) };
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut out = Vec::new();
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
    out
}

fn go_import_binding(clause: &str, targets: &[PathBuf]) -> Vec<ImportBinding> {
    let prefix = clause
        .split(['\'', '"'])
        .next()
        .unwrap_or("")
        .trim()
        .strip_prefix("import")
        .unwrap_or_else(|| clause.split(['\'', '"']).next().unwrap_or(""))
        .trim();
    if matches!(prefix, "." | "_") {
        return Vec::new();
    }
    let local_name: String = if prefix.is_empty() {
        let packages: BTreeSet<String> = targets
            .iter()
            .filter_map(|target| std::fs::read_to_string(target).ok())
            .filter_map(|source| {
                source.lines().find_map(|line| {
                    line.trim()
                        .strip_prefix("package ")
                        .and_then(|rest| rest.split_whitespace().next())
                        .map(str::to_string)
                })
            })
            .collect();
        if packages.len() != 1 {
            return Vec::new();
        }
        packages.iter().next().expect("one Go package name").clone()
    } else {
        prefix.split_whitespace().last().unwrap_or(prefix).to_string()
    };
    vec![ImportBinding {
        local_name,
        target_name: None,
        namespace: true,
        type_only: false,
    }]
}

fn push_go_import(
    mod_dir: &Path,
    mod_path: &str,
    clause: &str,
    spec: &str,
    line: usize,
    out: &mut Vec<ImportFact>,
) {
    let targets = go_pkg_targets(mod_dir, mod_path, spec);
    let bindings = go_import_binding(clause, &targets);
    for target in targets {
        out.push(ImportFact::new(target, line, line, bindings.clone()));
    }
}

fn go_edges(file: &Path, head: &[&str], root: &Path) -> Vec<ImportFact> {
    let Some((mod_dir, mod_path)) = go_module(file, root) else { return Vec::new() };
    let mut out = Vec::new();
    let mut i = 0;
    let mut in_block_comment = false;
    let mut in_import_block = false;
    while i < head.len() {
        let line_number = i + 1;
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
                push_go_import(
                    &mod_dir,
                    &mod_path,
                    line,
                    &spec,
                    line_number,
                    &mut out,
                );
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
                push_go_import(
                    &mod_dir,
                    &mod_path,
                    line,
                    &spec,
                    line_number,
                    &mut out,
                );
            }
            continue;
        }
        break;
    }
    out
}

// ------------------------------------------------------------ persistence

pub fn ensure_schema(conn: &Connection) -> anyhow::Result<()> {
    let current = conn
        .prepare("SELECT 1 FROM pragma_table_info('import_edges') WHERE name='start_line'")?
        .exists([])?;
    if !current {
        conn.execute_batch(
            "DROP TABLE IF EXISTS import_bindings;
             DROP TABLE IF EXISTS import_edges;",
        )?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS import_edges(
           src TEXT NOT NULL,
           dst TEXT NOT NULL,
           start_line INTEGER NOT NULL,
           end_line INTEGER NOT NULL,
           evidence TEXT NOT NULL,
           PRIMARY KEY(src, dst, start_line, end_line)
         );
         CREATE TABLE IF NOT EXISTS import_bindings(
           src TEXT NOT NULL,
           dst TEXT NOT NULL,
           start_line INTEGER NOT NULL,
           end_line INTEGER NOT NULL,
           local_name TEXT NOT NULL,
           target_name TEXT NOT NULL,
           kind TEXT NOT NULL,
           PRIMARY KEY(src, dst, start_line, end_line, local_name, target_name, kind)
         );
         CREATE INDEX IF NOT EXISTS import_edges_dst ON import_edges(dst);
         CREATE INDEX IF NOT EXISTS import_bindings_src ON import_bindings(src);",
    )?;
    Ok(())
}

/// Replaces the outgoing edges of one file. Keyed by path, not rowid:
/// `code_files` rows are re-inserted on change and rowids must not carry
/// meaning across scans.
pub(crate) fn replace_file_edges(
    conn: &Connection,
    src: &str,
    imports: &[ImportFact],
) -> anyhow::Result<()> {
    conn.execute("DELETE FROM import_bindings WHERE src=?1", [src])?;
    conn.execute("DELETE FROM import_edges WHERE src=?1", [src])?;
    for import in imports {
        let dst = import.target.to_string_lossy();
        conn.execute(
            "INSERT OR IGNORE INTO import_edges(src, dst, start_line, end_line, evidence)
             VALUES(?1, ?2, ?3, ?4, 'resolved')",
            rusqlite::params![src, dst, import.start_line as i64, import.end_line as i64],
        )?;
        for binding in &import.bindings {
            conn.execute(
                "INSERT OR IGNORE INTO import_bindings(
                   src, dst, start_line, end_line, local_name, target_name, kind
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    src,
                    dst,
                    import.start_line as i64,
                    import.end_line as i64,
                    binding.local_name,
                    binding.target_name.as_deref().unwrap_or(""),
                    match (binding.namespace, binding.type_only) {
                        (true, true) => "namespace-type",
                        (true, false) => "namespace",
                        (false, true) => "symbol-type",
                        (false, false) => "symbol",
                    },
                ],
            )?;
        }
    }
    Ok(())
}

/// Drops edges whose endpoints left the index (deleted or newly ignored
/// files) so the ranking never references ghosts.
pub fn prune_edges(conn: &Connection) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM import_bindings
         WHERE src NOT IN (SELECT path FROM code_files)
            OR dst NOT IN (SELECT path FROM code_files)",
        [],
    )?;
    conn.execute(
        "DELETE FROM import_edges
         WHERE src NOT IN (SELECT path FROM code_files)
            OR dst NOT IN (SELECT path FROM code_files)",
        [],
    )?;
    Ok(())
}

// --------------------------------------------- explainable dependency graph

pub const DEFAULT_PATH_DEPTH: usize = 12;
pub const DEFAULT_IMPACT_DEPTH: usize = 4;
pub const DEFAULT_IMPACT_LIMIT: usize = 50;
pub const DEFAULT_CONTEXT_DEPTH: usize = 1;
pub const DEFAULT_CONTEXT_LIMIT: usize = 50;
pub const MAX_DEPENDENCY_DEPTH: usize = 32;
pub const MAX_IMPACT_LIMIT: usize = 200;
pub const MAX_CONTEXT_LIMIT: usize = 200;
pub const DEFAULT_SYMBOL_LIMIT: usize = 50;
pub const MAX_SYMBOL_LIMIT: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SourceEvidence {
    pub class: String,
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
}

fn render_evidence(evidence: &SourceEvidence) -> String {
    format!(
        "{} {}:{}-{}",
        evidence.class, evidence.path, evidence.start_line, evidence.end_line
    )
}

/// One stable, typed explanation step from the rebuildable source graph.
///
/// Import extraction deliberately accepts only resolvable project-internal
/// imports in the source-file head and retains the exact source range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyEdge {
    pub source: String,
    pub relation: String,
    pub target: String,
    pub evidence: SourceEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyPath {
    pub from: String,
    pub to: String,
    pub max_depth: usize,
    pub found: bool,
    pub edges: Vec<DependencyEdge>,
}

/// One reverse dependency and the next file on its deterministic shortest
/// explanation toward the requested target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactNode {
    pub path: String,
    pub depth: usize,
    pub via: String,
    pub relation: String,
    pub evidence: SourceEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyImpact {
    pub target: String,
    pub max_depth: usize,
    pub total: usize,
    pub omitted: usize,
    pub nodes: Vec<ImpactNode>,
}

/// One file in a bounded bidirectional neighborhood. `edge` is the exact
/// directed import that first reached this file from the requested target;
/// it therefore explains the relationship without pretending the traversal
/// direction changes the source relationship.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextNode {
    pub path: String,
    pub depth: usize,
    pub edge: DependencyEdge,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyContext {
    pub target: String,
    pub max_depth: usize,
    pub total: usize,
    pub omitted: usize,
    pub nodes: Vec<ContextNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CodeGraphNode {
    pub node_kind: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TypedGraphEdge {
    pub source: CodeGraphNode,
    pub relation: String,
    pub target: CodeGraphNode,
    pub evidence: SourceEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolContext {
    pub query: String,
    pub total_symbols: usize,
    pub omitted_symbols: usize,
    pub symbols: Vec<CodeGraphNode>,
    pub total_edges: usize,
    pub omitted_edges: usize,
    pub edges: Vec<TypedGraphEdge>,
}

struct DependencyIndex {
    absolute: Vec<String>,
    display: Vec<String>,
    outgoing: Vec<Vec<usize>>,
    incoming: Vec<Vec<usize>>,
    evidence: BTreeMap<(usize, usize), SourceEvidence>,
}

fn portable_path(path: &Path) -> String {
    crate::index::rel_doc_path(path)
}

/// All served paths are root-relative and forward-slashed. When explicitly
/// configured roots contain the same relative path, `@N/` identifies the
/// root by its stable configuration order without leaking a host path.
fn dependency_index(conn: &Connection, roots: &[PathBuf]) -> anyhow::Result<DependencyIndex> {
    crate::code::ensure_schema(conn)?;
    anyhow::ensure!(!roots.is_empty(), "no code roots configured");

    let mut stmt = conn.prepare("SELECT path FROM code_files ORDER BY path")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut files: Vec<(String, usize, String)> = rows
        .filter_map(Result::ok)
        .filter_map(|absolute| {
            roots.iter().enumerate().find_map(|(root_index, root)| {
                Path::new(&absolute)
                    .strip_prefix(root)
                    .ok()
                    .map(|relative| (absolute.clone(), root_index, portable_path(relative)))
            })
        })
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files.dedup_by(|a, b| a.0 == b.0);
    anyhow::ensure!(!files.is_empty(), "code index is empty for the configured roots — run `cfetch scan`");

    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, _, relative) in &files {
        *counts.entry(relative).or_default() += 1;
    }
    let absolute: Vec<String> = files.iter().map(|(path, _, _)| path.clone()).collect();
    let display: Vec<String> = files
        .iter()
        .map(|(_, root_index, relative)| {
            if counts.get(relative.as_str()).copied().unwrap_or(0) > 1 {
                format!("@{}/{relative}", root_index + 1)
            } else {
                relative.clone()
            }
        })
        .collect();
    let by_absolute: BTreeMap<&str, usize> =
        absolute.iter().enumerate().map(|(index, path)| (path.as_str(), index)).collect();
    let mut outgoing = vec![Vec::new(); absolute.len()];
    let mut incoming = vec![Vec::new(); absolute.len()];
    let mut evidence = BTreeMap::new();
    let mut edges = conn.prepare(
        "SELECT src, dst, start_line, end_line, evidence
         FROM import_edges ORDER BY src, dst, start_line, end_line",
    )?;
    let rows = edges.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)? as usize,
            row.get::<_, i64>(3)? as usize,
            row.get::<_, String>(4)?,
        ))
    })?;
    for (source, target, start_line, end_line, class) in rows.filter_map(Result::ok) {
        let (Some(&source), Some(&target)) =
            (by_absolute.get(source.as_str()), by_absolute.get(target.as_str()))
        else {
            continue;
        };
        outgoing[source].push(target);
        incoming[target].push(source);
        evidence.entry((source, target)).or_insert_with(|| SourceEvidence {
            class,
            path: display[source].clone(),
            start_line,
            end_line,
        });
    }
    for neighbors in outgoing.iter_mut().chain(incoming.iter_mut()) {
        neighbors.sort_by(|left, right| display[*left].cmp(&display[*right]));
        neighbors.dedup();
    }
    Ok(DependencyIndex { absolute, display, outgoing, incoming, evidence })
}

fn normalized_query(query: &str) -> String {
    query
        .trim()
        .replace('\\', "/")
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_string()
}

fn suffix_match(candidate: &str, query: &str) -> bool {
    candidate == query
        || candidate
            .strip_suffix(query)
            .is_some_and(|prefix| prefix.ends_with('/'))
}

fn resolve_file(index: &DependencyIndex, query: &str) -> anyhow::Result<usize> {
    let query = normalized_query(query);
    anyhow::ensure!(!query.is_empty(), "dependency target must name a file path");

    let portable_absolute: Vec<String> = index
        .absolute
        .iter()
        .map(|path| path.replace('\\', "/"))
        .collect();
    let mut matches: Vec<usize> = index
        .display
        .iter()
        .zip(&portable_absolute)
        .enumerate()
        .filter_map(|(i, (display, absolute))| {
            (display == &query || absolute == &query).then_some(i)
        })
        .collect();
    if matches.is_empty() {
        matches = index
            .display
            .iter()
            .zip(&portable_absolute)
            .enumerate()
            .filter_map(|(i, (display, absolute))| {
                (suffix_match(display, &query) || suffix_match(absolute, &query)).then_some(i)
            })
            .collect();
    }
    if matches.is_empty() {
        let folded = query.to_ascii_lowercase();
        matches = index
            .display
            .iter()
            .zip(&portable_absolute)
            .enumerate()
            .filter_map(|(i, (display, absolute))| {
                let display = display.to_ascii_lowercase();
                let absolute = absolute.to_ascii_lowercase();
                (suffix_match(&display, &folded) || suffix_match(&absolute, &folded)).then_some(i)
            })
            .collect();
    }
    matches.sort_unstable();
    matches.dedup();
    match matches.as_slice() {
        [only] => Ok(*only),
        [] => anyhow::bail!("no indexed code file matches {query:?}"),
        _ => {
            let mut candidates: Vec<&str> =
                matches.iter().map(|&i| index.display[i].as_str()).collect();
            candidates.sort_unstable();
            anyhow::bail!(
                "dependency target {query:?} is ambiguous; use one of: {}",
                candidates.join(", ")
            )
        }
    }
}

fn dependency_edge(index: &DependencyIndex, source: usize, target: usize) -> DependencyEdge {
    DependencyEdge {
        source: index.display[source].clone(),
        relation: "imports".to_string(),
        target: index.display[target].clone(),
        evidence: index
            .evidence
            .get(&(source, target))
            .cloned()
            .expect("every dependency edge has persisted evidence"),
    }
}

/// Finds one deterministic shortest directed import path. Equal-length paths
/// are settled in lexical display-path order, so every host serving the same
/// catalog returns the same explanation.
pub fn dependency_path(
    conn: &Connection,
    roots: &[PathBuf],
    from: &str,
    to: &str,
    max_depth: usize,
) -> anyhow::Result<DependencyPath> {
    anyhow::ensure!(
        (1..=MAX_DEPENDENCY_DEPTH).contains(&max_depth),
        "max depth must be between 1 and {MAX_DEPENDENCY_DEPTH}"
    );
    let index = dependency_index(conn, roots)?;
    let from = resolve_file(&index, from)?;
    let to = resolve_file(&index, to)?;
    let mut previous = vec![None; index.display.len()];
    let mut depth = vec![usize::MAX; index.display.len()];
    let mut queue = VecDeque::from([from]);
    depth[from] = 0;
    while let Some(node) = queue.pop_front() {
        if node == to || depth[node] >= max_depth {
            continue;
        }
        for &next in &index.outgoing[node] {
            if depth[next] != usize::MAX {
                continue;
            }
            depth[next] = depth[node] + 1;
            previous[next] = Some(node);
            queue.push_back(next);
        }
    }
    let mut edges = Vec::new();
    if depth[to] != usize::MAX {
        let mut nodes = vec![to];
        let mut cursor = to;
        while cursor != from {
            cursor = previous[cursor].expect("a reached dependency node has a predecessor");
            nodes.push(cursor);
        }
        nodes.reverse();
        edges = nodes
            .windows(2)
            .map(|pair| dependency_edge(&index, pair[0], pair[1]))
            .collect();
    }
    Ok(DependencyPath {
        from: index.display[from].clone(),
        to: index.display[to].clone(),
        max_depth,
        found: depth[to] != usize::MAX,
        edges,
    })
}

/// Walks import edges backwards to show which indexed files can be affected
/// by a target. Every result carries the next hop toward the target, making
/// the blast radius inspectable rather than a bag of graph scores.
pub fn dependency_impact(
    conn: &Connection,
    roots: &[PathBuf],
    target: &str,
    max_depth: usize,
    limit: usize,
) -> anyhow::Result<DependencyImpact> {
    anyhow::ensure!(
        (1..=MAX_DEPENDENCY_DEPTH).contains(&max_depth),
        "max depth must be between 1 and {MAX_DEPENDENCY_DEPTH}"
    );
    anyhow::ensure!(
        (1..=MAX_IMPACT_LIMIT).contains(&limit),
        "limit must be between 1 and {MAX_IMPACT_LIMIT}"
    );
    let index = dependency_index(conn, roots)?;
    let target = resolve_file(&index, target)?;
    let mut via = vec![None; index.display.len()];
    let mut depth = vec![usize::MAX; index.display.len()];
    let mut queue = VecDeque::from([target]);
    depth[target] = 0;
    while let Some(node) = queue.pop_front() {
        if depth[node] >= max_depth {
            continue;
        }
        for &importer in &index.incoming[node] {
            if depth[importer] != usize::MAX {
                continue;
            }
            depth[importer] = depth[node] + 1;
            via[importer] = Some(node);
            queue.push_back(importer);
        }
    }
    let mut reached: Vec<usize> = (0..index.display.len())
        .filter(|&node| node != target && depth[node] != usize::MAX)
        .collect();
    reached.sort_by(|&left, &right| {
        depth[left]
            .cmp(&depth[right])
            .then(index.display[left].cmp(&index.display[right]))
    });
    let total = reached.len();
    reached.truncate(limit);
    let nodes = reached
        .into_iter()
        .map(|node| ImpactNode {
            path: index.display[node].clone(),
            depth: depth[node],
            via: index.display[via[node].expect("a reverse-reached node has a next hop")].clone(),
            relation: "imports".to_string(),
            evidence: index
                .evidence
                .get(&(node, via[node].expect("a reverse-reached node has a next hop")))
                .cloned()
                .expect("every dependency edge has persisted evidence"),
        })
        .collect();
    Ok(DependencyImpact {
        target: index.display[target].clone(),
        max_depth,
        total,
        omitted: total.saturating_sub(limit),
        nodes,
    })
}

/// Builds a bounded neighborhood around one file by traversing imports in
/// both directions. Every reached file retains exactly one deterministic
/// shortest explanation edge, keeping the result linear in the node limit
/// instead of returning an unbounded induced subgraph.
pub fn dependency_context(
    conn: &Connection,
    roots: &[PathBuf],
    target: &str,
    max_depth: usize,
    limit: usize,
) -> anyhow::Result<DependencyContext> {
    anyhow::ensure!(
        (1..=MAX_DEPENDENCY_DEPTH).contains(&max_depth),
        "max depth must be between 1 and {MAX_DEPENDENCY_DEPTH}"
    );
    anyhow::ensure!(
        (1..=MAX_CONTEXT_LIMIT).contains(&limit),
        "limit must be between 1 and {MAX_CONTEXT_LIMIT}"
    );
    let index = dependency_index(conn, roots)?;
    let target = resolve_file(&index, target)?;
    let mut explanation = vec![None; index.display.len()];
    let mut depth = vec![usize::MAX; index.display.len()];
    let mut queue = VecDeque::from([target]);
    depth[target] = 0;
    while let Some(node) = queue.pop_front() {
        if depth[node] >= max_depth {
            continue;
        }
        // Sort both traversal directions together by the newly reached file,
        // then by the real directed edge. This also settles two-way imports
        // identically on every platform.
        let mut neighbors: Vec<(usize, usize, usize)> = index.outgoing[node]
            .iter()
            .map(|&neighbor| (neighbor, node, neighbor))
            .chain(index.incoming[node].iter().map(|&neighbor| (neighbor, neighbor, node)))
            .collect();
        neighbors.sort_by(|left, right| {
            index.display[left.0]
                .cmp(&index.display[right.0])
                .then(index.display[left.1].cmp(&index.display[right.1]))
                .then(index.display[left.2].cmp(&index.display[right.2]))
        });
        neighbors.dedup();
        for (neighbor, source, imported) in neighbors {
            if depth[neighbor] != usize::MAX {
                continue;
            }
            depth[neighbor] = depth[node] + 1;
            explanation[neighbor] = Some((source, imported));
            queue.push_back(neighbor);
        }
    }
    let mut reached: Vec<usize> = (0..index.display.len())
        .filter(|&node| node != target && depth[node] != usize::MAX)
        .collect();
    reached.sort_by(|&left, &right| {
        depth[left]
            .cmp(&depth[right])
            .then(index.display[left].cmp(&index.display[right]))
    });
    let total = reached.len();
    reached.truncate(limit);
    let nodes = reached
        .into_iter()
        .map(|node| {
            let (source, imported) =
                explanation[node].expect("a context-reached node has an explanation edge");
            ContextNode {
                path: index.display[node].clone(),
                depth: depth[node],
                edge: dependency_edge(&index, source, imported),
            }
        })
        .collect();
    Ok(DependencyContext {
        target: index.display[target].clone(),
        max_depth,
        total,
        omitted: total.saturating_sub(limit),
        nodes,
    })
}

#[derive(Clone)]
struct StoredSymbol {
    file: String,
    name: String,
    kind: String,
    start_line: usize,
    end_line: usize,
    parent_start_line: Option<usize>,
}

impl StoredSymbol {
    fn identity(&self) -> (&str, usize, &str) {
        (&self.file, self.start_line, &self.name)
    }

    fn node(&self, displays: &BTreeMap<&str, &str>) -> CodeGraphNode {
        CodeGraphNode {
            node_kind: "symbol".to_string(),
            path: displays.get(self.file.as_str()).copied().unwrap_or(&self.file).to_string(),
            name: Some(self.name.clone()),
            symbol_kind: Some(self.kind.clone()),
            start_line: Some(self.start_line),
            end_line: Some(self.end_line),
        }
    }
}

struct StoredUse {
    file: String,
    name: String,
    qualifier: Option<String>,
    relation: String,
    start_line: usize,
    end_line: usize,
    container_start_line: usize,
}

type DirectBindings<'a> = BTreeMap<(&'a str, &'a str), Vec<(&'a str, &'a str, bool)>>;
type NamespaceBindings<'a> = BTreeMap<(&'a str, &'a str), Vec<(&'a str, bool)>>;

/// Returns parser-proven symbol relationships around an exact symbol name.
/// A call/reference resolves only through an explicit import binding and only
/// when that binding leads to exactly one file-level definition. Ambiguity or
/// grammar uncertainty therefore removes an edge instead of inventing one.
pub fn symbol_context(
    conn: &Connection,
    roots: &[PathBuf],
    query: &str,
    limit: usize,
) -> anyhow::Result<SymbolContext> {
    anyhow::ensure!(
        (1..=MAX_SYMBOL_LIMIT).contains(&limit),
        "limit must be between 1 and {MAX_SYMBOL_LIMIT}"
    );
    let normalized = crate::code::norm_ident(query);
    anyhow::ensure!(!normalized.is_empty(), "symbol query must contain a name");
    let index = dependency_index(conn, roots)?;
    let displays: BTreeMap<&str, &str> = index
        .absolute
        .iter()
        .zip(&index.display)
        .map(|(absolute, display)| (absolute.as_str(), display.as_str()))
        .collect();

    let mut statement = conn.prepare(
        "SELECT f.path, s.name, s.kind, s.start_line, s.end_line, s.parent_start_line
         FROM symbols s JOIN code_files f ON f.id=s.file_id
         ORDER BY f.path, s.start_line, s.name",
    )?;
    let symbols: Vec<StoredSymbol> = statement
        .query_map([], |row| {
            Ok(StoredSymbol {
                file: row.get(0)?,
                name: row.get(1)?,
                kind: row.get(2)?,
                start_line: row.get::<_, i64>(3)? as usize,
                end_line: row.get::<_, i64>(4)? as usize,
                parent_start_line: row.get::<_, Option<i64>>(5)?.map(|line| line as usize),
            })
        })?
        .filter_map(Result::ok)
        .filter(|symbol| displays.contains_key(symbol.file.as_str()))
        .collect();
    let matched_all: Vec<StoredSymbol> = symbols
        .iter()
        .filter(|symbol| crate::code::norm_ident(&symbol.name) == normalized)
        .cloned()
        .collect();
    let total_symbols = matched_all.len();
    let mut matched = matched_all.clone();
    matched.truncate(limit);
    let matched_ids: BTreeSet<(String, usize, String)> = matched_all
        .iter()
        .map(|symbol| (symbol.file.clone(), symbol.start_line, symbol.name.clone()))
        .collect();
    let nodes: Vec<CodeGraphNode> = matched.iter().map(|symbol| symbol.node(&displays)).collect();

    let mut edges: BTreeSet<TypedGraphEdge> = BTreeSet::new();
    for symbol in &matched_all {
        let path = displays.get(symbol.file.as_str()).copied().unwrap_or(&symbol.file);
        edges.insert(TypedGraphEdge {
            source: CodeGraphNode {
                node_kind: "file".to_string(),
                path: path.to_string(),
                name: None,
                symbol_kind: None,
                start_line: None,
                end_line: None,
            },
            relation: "contains".to_string(),
            target: symbol.node(&displays),
            evidence: SourceEvidence {
                class: "extracted".to_string(),
                path: path.to_string(),
                start_line: symbol.start_line,
                end_line: symbol.end_line,
            },
        });
    }

    let mut direct = DirectBindings::new();
    let mut namespaces = NamespaceBindings::new();
    let mut bindings = conn.prepare(
        "SELECT src, dst, local_name, target_name, kind
         FROM import_bindings ORDER BY src, local_name, dst, target_name",
    )?;
    let binding_rows = bindings.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    let binding_rows: Vec<(String, String, String, String, String)> =
        binding_rows.filter_map(Result::ok).collect();
    for (src, dst, local, target, kind) in &binding_rows {
        let type_only = kind.ends_with("-type");
        if kind.starts_with("namespace") {
            namespaces.entry((src, local)).or_default().push((dst, type_only));
        } else {
            direct.entry((src, local)).or_default().push((dst, target, type_only));
        }
    }
    let mut top_level: BTreeMap<(&str, &str), Vec<&StoredSymbol>> = BTreeMap::new();
    // Same-start-line collision: one-line nested symbols (`class A: def
    // m(self): f()` both start at line 1). walk_uses attributes to the
    // INNERMOST named symbol; last-writer-wins under name sort gave the
    // wrong one whenever the outer sorted later. Prefer the symbol with the
    // LARGEST end_line at the same start_line (the innermost scope).
    let mut containers: BTreeMap<(&str, usize), &StoredSymbol> = BTreeMap::new();
    for symbol in &symbols {
        let key = (symbol.file.as_str(), symbol.start_line);
        match containers.get(&key) {
            Some(existing) if existing.end_line > symbol.end_line => {}
            _ => { containers.insert(key, symbol); }
        }
        if symbol.parent_start_line.is_none() {
            top_level.entry((symbol.file.as_str(), symbol.name.as_str())).or_default().push(symbol);
        }
    }

    let mut uses = conn.prepare(
        "SELECT f.path, u.name, u.qualifier, u.relation, u.start_line, u.end_line,
                u.container_start_line
         FROM symbol_uses u JOIN code_files f ON f.id=u.file_id
         ORDER BY f.path, u.start_line, u.end_line, u.relation, u.name",
    )?;
    let uses = uses.query_map([], |row| {
        Ok(StoredUse {
            file: row.get(0)?,
            name: row.get(1)?,
            qualifier: row.get(2)?,
            relation: row.get(3)?,
            start_line: row.get::<_, i64>(4)? as usize,
            end_line: row.get::<_, i64>(5)? as usize,
            container_start_line: row.get::<_, i64>(6)? as usize,
        })
    })?;
    for usage in uses.filter_map(Result::ok) {
        let Some(source) = containers.get(&(usage.file.as_str(), usage.container_start_line)) else {
            continue;
        };
        let mut candidates: Vec<&StoredSymbol> = Vec::new();
        match usage.qualifier.as_deref() {
            Some(qualifier) => {
                for (dst, type_only) in namespaces
                    .get(&(usage.file.as_str(), qualifier))
                    .into_iter()
                    .flatten()
                {
                    if *type_only && usage.relation != "references" {
                        continue;
                    }
                    candidates.extend(
                        top_level
                            .get(&(*dst, usage.name.as_str()))
                            .into_iter()
                            .flatten()
                            .copied(),
                    );
                }
            }
            None => {
                for (dst, target_name, type_only) in direct
                    .get(&(usage.file.as_str(), usage.name.as_str()))
                    .into_iter()
                    .flatten()
                {
                    if *type_only && usage.relation != "references" {
                        continue;
                    }
                    candidates.extend(
                        top_level
                            .get(&(*dst, *target_name))
                            .into_iter()
                            .flatten()
                            .copied(),
                    );
                }
            }
        }
        candidates.sort_by_key(|candidate| candidate.identity());
        candidates.dedup_by_key(|candidate| candidate.identity());
        let [target] = candidates.as_slice() else { continue };
        let source_id = (source.file.clone(), source.start_line, source.name.clone());
        let target_id = (target.file.clone(), target.start_line, target.name.clone());
        if !matched_ids.contains(&source_id) && !matched_ids.contains(&target_id) {
            continue;
        }
        let evidence_path = displays
            .get(usage.file.as_str())
            .copied()
            .unwrap_or(usage.file.as_str());
        edges.insert(TypedGraphEdge {
            source: source.node(&displays),
            relation: usage.relation,
            target: target.node(&displays),
            evidence: SourceEvidence {
                class: "resolved".to_string(),
                path: evidence_path.to_string(),
                start_line: usage.start_line,
                end_line: usage.end_line,
            },
        });
    }
    let total_edges = edges.len();
    let kept_ids: BTreeSet<(String, usize, String)> = nodes
        .iter()
        .filter_map(|node| {
            Some((
                node.path.clone(),
                node.start_line?,
                node.name.clone()?,
            ))
        })
        .collect();
    let incident_to_kept = |node: &CodeGraphNode| {
        node.node_kind == "symbol"
            && node
                .start_line
                .zip(node.name.as_ref())
                .is_some_and(|(line, name)| kept_ids.contains(&(node.path.clone(), line, name.clone())))
    };
    let edges: Vec<TypedGraphEdge> = edges
        .into_iter()
        .filter(|edge| incident_to_kept(&edge.source) || incident_to_kept(&edge.target))
        .take(limit)
        .collect();
    Ok(SymbolContext {
        query: query.to_string(),
        total_symbols,
        omitted_symbols: total_symbols.saturating_sub(nodes.len()),
        symbols: nodes,
        total_edges,
        omitted_edges: total_edges.saturating_sub(edges.len()),
        edges,
    })
}

pub fn render_symbol_context(context: &SymbolContext) -> String {
    let mut lines = vec![format!(
        "symbol context: {:?} matched {} symbol(s), {} edge(s)",
        context.query, context.total_symbols, context.total_edges
    )];
    lines.extend(context.edges.iter().map(|edge| {
        let source = edge.source.name.as_deref().unwrap_or(&edge.source.path);
        let target = edge.target.name.as_deref().unwrap_or(&edge.target.path);
        format!(
            "{}#{} --{}--> {}#{} [{}]",
            edge.source.path,
            source,
            edge.relation,
            edge.target.path,
            target,
            render_evidence(&edge.evidence)
        )
    }));
    if context.omitted_symbols + context.omitted_edges > 0 {
        lines.push(format!(
            "... {} symbol(s) and {} edge(s) omitted by the limit",
            context.omitted_symbols, context.omitted_edges
        ));
    }
    lines.join("\n")
}

pub fn render_dependency_path(path: &DependencyPath) -> String {
    if !path.found {
        return format!(
            "no import path from {} to {} within {} hop(s)",
            path.from, path.to, path.max_depth
        );
    }
    if path.edges.is_empty() {
        return format!("dependency path: {} is the requested target (0 hops)", path.from);
    }
    let mut lines = vec![format!(
        "dependency path: {} -> {} ({} hop(s))",
        path.from,
        path.to,
        path.edges.len()
    )];
    lines.extend(path.edges.iter().map(|edge| {
        format!(
            "{} --{}--> {} [{}]",
            edge.source, edge.relation, edge.target, render_evidence(&edge.evidence)
        )
    }));
    lines.join("\n")
}

pub fn render_dependency_impact(impact: &DependencyImpact) -> String {
    let mut lines = vec![format!(
        "dependency impact: {} <- {} file(s), depth <= {}",
        impact.target, impact.total, impact.max_depth
    )];
    lines.extend(impact.nodes.iter().map(|node| {
        format!(
            "d{} {} --{}--> {} [{}]",
            node.depth,
            node.path,
            node.relation,
            node.via,
            render_evidence(&node.evidence)
        )
    }));
    if impact.omitted > 0 {
        lines.push(format!("... {} more file(s) omitted by the limit", impact.omitted));
    }
    lines.join("\n")
}

pub fn render_dependency_context(context: &DependencyContext) -> String {
    let mut lines = vec![format!(
        "dependency context: {} with {} related file(s), depth <= {}",
        context.target, context.total, context.max_depth
    )];
    lines.extend(context.nodes.iter().map(|node| {
        format!(
            "d{} {} --{}--> {} [{}]",
            node.depth,
            node.edge.source,
            node.edge.relation,
            node.edge.target,
            render_evidence(&node.edge.evidence)
        )
    }));
    if context.omitted > 0 {
        lines.push(format!("... {} more file(s) omitted by the limit", context.omitted));
    }
    lines.join("\n")
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
    let mut stmt = conn.prepare("SELECT DISTINCT src, dst FROM import_edges")?;
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

/// Default token budget for a rendered repo map. Shared by the CLI flag and
/// the serving protocol's `map` op so both sides render the same map when the
/// caller says nothing.
pub const DEFAULT_MAP_BUDGET_TOKENS: u64 = 1500;

#[derive(Debug, Clone)]
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
            // Forward slashes on every platform. Map lines are SERVED: a
            // Windows host and a Linux host indexing the same repository must
            // hand a client byte-identical lines, or the same file reads as
            // two different files depending on who answered.
            let rel = Path::new(&p)
                .strip_prefix(root)
                .map(crate::index::rel_doc_path)
                .unwrap_or_else(|_| crate::index::rel_doc_path(Path::new(&p)));
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

    /// Map lines cross the wire. A Windows host answering `map` for the same
    /// repository must produce the SAME bytes a Linux host does, or one file
    /// reads as two depending on which host was asked. Caught by the Windows
    /// CI runner, which rendered `proj\src\lib.rs`.
    #[test]
    fn map_paths_are_forward_slashed_on_every_platform() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/deep/nested.rs", "pub fn only_symbol() {}\n");
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        crate::code::scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();

        let m = map(&conn, &[dir.path().to_path_buf()], None, 100_000).unwrap();
        assert!(
            m.lines.iter().any(|l| l.starts_with("src/deep/nested.rs ")),
            "nested path must render with forward slashes: {:?}",
            m.lines
        );
        assert!(
            !m.lines.iter().any(|l| l.contains('\\')),
            "no native separator may leak into a served line: {:?}",
            m.lines
        );
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

    // ---- explainable dependency queries

    fn branching_rust_project(root: &Path) {
        write(root, "main.rs", "mod a;\nmod z;\nfn main() {}\n");
        write(root, "a.rs", "use crate::b::value;\npub fn a() {}\n");
        write(root, "z.rs", "use crate::b::value;\npub fn z() {}\n");
        write(root, "b.rs", "pub fn value() {}\n");
    }

    #[test]
    fn dependency_path_is_shortest_typed_and_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        branching_rust_project(dir.path());
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        crate::code::scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();

        let path = dependency_path(
            &conn,
            &[dir.path().to_path_buf()],
            "main.rs",
            "b.rs",
            8,
        )
        .unwrap();
        assert!(path.found);
        assert_eq!(path.from, "main.rs");
        assert_eq!(path.to, "b.rs");
        assert_eq!(path.edges.len(), 2);
        assert_eq!(path.edges[0].source, "main.rs");
        assert_eq!(path.edges[0].target, "a.rs", "equal shortest paths use lexical order");
        assert_eq!(path.edges[0].relation, "imports");
        assert_eq!(path.edges[0].evidence.class, "resolved");
        assert_eq!(path.edges[0].evidence.path, "main.rs");
        assert_eq!((path.edges[0].evidence.start_line, path.edges[0].evidence.end_line), (1, 1));
        assert_eq!(path.edges[1].source, "a.rs");
        assert_eq!(path.edges[1].target, "b.rs");

        let bounded = dependency_path(
            &conn,
            &[dir.path().to_path_buf()],
            "main.rs",
            "b.rs",
            1,
        )
        .unwrap();
        assert!(!bounded.found, "the bound must be real, not advisory");
        assert!(bounded.edges.is_empty());
    }

    #[test]
    fn dependency_impact_walks_reverse_edges_with_explainable_steps() {
        let dir = tempfile::tempdir().unwrap();
        branching_rust_project(dir.path());
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        crate::code::scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();

        let impact = dependency_impact(
            &conn,
            &[dir.path().to_path_buf()],
            "b.rs",
            4,
            10,
        )
        .unwrap();
        assert_eq!(impact.target, "b.rs");
        assert_eq!(impact.total, 3);
        assert_eq!(impact.omitted, 0);
        assert_eq!(
            impact.nodes.iter().map(|node| (&*node.path, node.depth)).collect::<Vec<_>>(),
            vec![("a.rs", 1), ("z.rs", 1), ("main.rs", 2)]
        );
        let main = impact.nodes.iter().find(|node| node.path == "main.rs").unwrap();
        assert_eq!(main.via, "a.rs", "the lexical shortest explanation is stable");
        assert_eq!(main.relation, "imports");
        assert_eq!(main.evidence.class, "resolved");

        let clipped = dependency_impact(
            &conn,
            &[dir.path().to_path_buf()],
            "b.rs",
            4,
            2,
        )
        .unwrap();
        assert_eq!(clipped.nodes.len(), 2);
        assert_eq!(clipped.omitted, 1, "bounded answers must name what they omit");
    }

    #[test]
    fn dependency_context_walks_both_directions_with_one_explanation_per_file() {
        let dir = tempfile::tempdir().unwrap();
        branching_rust_project(dir.path());
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        crate::code::scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();

        let context = dependency_context(
            &conn,
            &[dir.path().to_path_buf()],
            "a.rs",
            2,
            10,
        )
        .unwrap();
        assert_eq!(context.target, "a.rs");
        assert_eq!(context.total, 3);
        assert_eq!(context.omitted, 0);
        assert_eq!(
            context.nodes.iter().map(|node| (&*node.path, node.depth)).collect::<Vec<_>>(),
            vec![("b.rs", 1), ("main.rs", 1), ("z.rs", 2)]
        );

        let imported = context.nodes.iter().find(|node| node.path == "b.rs").unwrap();
        assert_eq!(imported.edge.source, "a.rs");
        assert_eq!(imported.edge.target, "b.rs");
        assert_eq!(imported.edge.relation, "imports");
        assert_eq!(imported.edge.evidence.class, "resolved");

        let importer = context.nodes.iter().find(|node| node.path == "main.rs").unwrap();
        assert_eq!(importer.edge.source, "main.rs");
        assert_eq!(importer.edge.target, "a.rs");

        let transitive = context.nodes.iter().find(|node| node.path == "z.rs").unwrap();
        assert_eq!(transitive.edge.source, "z.rs");
        assert_eq!(
            transitive.edge.target, "b.rs",
            "equal shortest explanations must settle lexically"
        );

        let direct = dependency_context(
            &conn,
            &[dir.path().to_path_buf()],
            "a.rs",
            1,
            10,
        )
        .unwrap();
        assert_eq!(direct.total, 2, "the depth bound must be real, not advisory");
        assert!(direct.nodes.iter().all(|node| node.depth == 1));

        let clipped = dependency_context(
            &conn,
            &[dir.path().to_path_buf()],
            "a.rs",
            2,
            2,
        )
        .unwrap();
        assert_eq!(clipped.nodes.len(), 2);
        assert_eq!(clipped.omitted, 1, "bounded answers must name what they omit");
    }

    #[test]
    fn dependency_import_evidence_keeps_the_exact_multiline_statement_range() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "lib.rs",
            "use crate::worker::{\n    run,\n};\npub fn caller() { run(); }\n",
        );
        write(dir.path(), "worker.rs", "pub fn run() {}\n");
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        crate::code::scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();

        let path = dependency_path(
            &conn,
            &[dir.path().to_path_buf()],
            "lib.rs",
            "worker.rs",
            2,
        )
        .unwrap();
        assert!(path.found);
        assert_eq!(path.edges.len(), 1);
        assert_eq!(path.edges[0].evidence.path, "lib.rs");
        assert_eq!((path.edges[0].evidence.start_line, path.edges[0].evidence.end_line), (1, 3));
    }

    #[test]
    fn symbol_context_resolves_only_parser_uses_bound_by_explicit_imports() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "lib.rs",
            "mod worker;\nuse crate::worker::{run, Thing};\npub fn caller(input: Thing) {\n    run();\n}\n",
        );
        write(dir.path(), "worker.rs", "pub struct Thing;\npub fn run() {}\n");
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        crate::code::scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();

        let context = symbol_context(
            &conn,
            &[dir.path().to_path_buf()],
            "caller",
            20,
        )
        .unwrap();
        assert_eq!(context.total_symbols, 1);
        assert_eq!(context.symbols[0].name.as_deref(), Some("caller"));
        let contains = context.edges.iter().find(|edge| edge.relation == "contains").unwrap();
        assert_eq!(contains.source.node_kind, "file");
        assert_eq!(contains.target.name.as_deref(), Some("caller"));
        assert_eq!(contains.evidence.class, "extracted");
        assert_eq!((contains.evidence.start_line, contains.evidence.end_line), (3, 5));

        let call = context.edges.iter().find(|edge| edge.relation == "calls").unwrap();
        assert_eq!(call.source.name.as_deref(), Some("caller"));
        assert_eq!(call.target.name.as_deref(), Some("run"));
        assert_eq!(call.target.path, "worker.rs");
        assert_eq!(call.evidence.class, "resolved");
        assert_eq!((call.evidence.start_line, call.evidence.end_line), (4, 4));

        let reference = context.edges.iter().find(|edge| edge.relation == "references").unwrap();
        assert_eq!(reference.source.name.as_deref(), Some("caller"));
        assert_eq!(reference.target.name.as_deref(), Some("Thing"));
        assert_eq!(reference.target.path, "worker.rs");
        assert_eq!((reference.evidence.start_line, reference.evidence.end_line), (3, 3));
    }

    #[test]
    fn ambiguous_imported_symbol_uses_produce_no_invented_edge() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "lib.rs",
            "mod a;\nmod b;\nuse crate::a::run;\nuse crate::b::run;\npub fn caller() { run(); }\n",
        );
        write(dir.path(), "a.rs", "pub fn run() {}\n");
        write(dir.path(), "b.rs", "pub fn run() {}\n");
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        crate::code::scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();

        let context = symbol_context(
            &conn,
            &[dir.path().to_path_buf()],
            "caller",
            20,
        )
        .unwrap();
        assert!(
            context.edges.iter().all(|edge| edge.relation != "calls"),
            "an ambiguous name must remain unresolved: {:?}",
            context.edges
        );
    }

    #[test]
    fn symbol_context_counts_relationships_hidden_with_omitted_symbols() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "pub fn caller() {}\n");
        write(dir.path(), "b.rs", "pub fn caller() {}\n");
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        crate::code::scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();

        let context = symbol_context(
            &conn,
            &[dir.path().to_path_buf()],
            "caller",
            1,
        )
        .unwrap();
        assert_eq!((context.total_symbols, context.symbols.len(), context.omitted_symbols), (2, 1, 1));
        assert_eq!((context.total_edges, context.edges.len(), context.omitted_edges), (2, 1, 1));
    }

    #[test]
    fn typescript_type_only_imports_never_invent_value_calls() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "app.ts",
            "import type { Thing } from './worker';\nexport function caller(input: Thing) { Thing(); }\n",
        );
        write(dir.path(), "worker.ts", "export class Thing {}\n");
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        crate::code::scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();

        let context = symbol_context(
            &conn,
            &[dir.path().to_path_buf()],
            "caller",
            10,
        )
        .unwrap();
        assert!(context.edges.iter().any(|edge| edge.relation == "references"));
        assert!(context.edges.iter().all(|edge| edge.relation != "calls"));
    }

    #[test]
    fn explicit_calls_resolve_across_every_supported_parser_family() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "ts/app.ts",
            "import { run } from './worker';\nexport function caller() { run(); }\n",
        );
        write(dir.path(), "ts/worker.ts", "export function run() {}\n");
        write(
            dir.path(),
            "js/app.js",
            "import { run } from './worker.js';\nexport function caller() { run(); }\n",
        );
        write(dir.path(), "js/worker.js", "export function run() {}\n");
        write(
            dir.path(),
            "py/app.py",
            "from worker import run\ndef caller():\n    run()\n",
        );
        write(dir.path(), "py/worker.py", "def run():\n    pass\n");
        write(dir.path(), "go/go.mod", "module example\n");
        write(
            dir.path(),
            "go/app.go",
            "package app\nimport \"example/worker\"\nfunc caller() { worker.Run() }\n",
        );
        write(
            dir.path(),
            "go/worker/worker.go",
            "package worker\nfunc Run() {}\n",
        );
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        crate::code::scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();

        let context = symbol_context(
            &conn,
            &[dir.path().to_path_buf()],
            "caller",
            20,
        )
        .unwrap();
        let targets: BTreeSet<&str> = context
            .edges
            .iter()
            .filter(|edge| edge.relation == "calls")
            .map(|edge| edge.target.path.as_str())
            .collect();
        assert_eq!(
            targets,
            BTreeSet::from(["go/worker/worker.go", "js/worker.js", "py/worker.py", "ts/worker.ts"])
        );
    }

    #[test]
    fn dependency_targets_never_guess_across_ambiguous_suffixes() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "one/lib.rs", "pub fn one() {}\n");
        write(dir.path(), "two/lib.rs", "pub fn two() {}\n");
        let state = tempfile::tempdir().unwrap();
        let mut conn = crate::index::open(state.path()).unwrap();
        crate::code::scan_code(&mut conn, &[dir.path().to_path_buf()]).unwrap();

        let error = dependency_impact(
            &conn,
            &[dir.path().to_path_buf()],
            "lib.rs",
            2,
            10,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("ambiguous"), "{error}");
        assert!(error.contains("one/lib.rs"), "{error}");
        assert!(error.contains("two/lib.rs"), "{error}");
    }
}
