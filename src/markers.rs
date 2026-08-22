//! Marker-block protocol for instruction files cfetch does not own
//! (AGENTS.md, GEMINI.md): exactly one delimited block is upserted; every
//! byte outside the markers is preserved. Broken markers mean REFUSE — a
//! half-written foreign file is more dangerous repaired than left alone.
//!
//! The block's content is the recall-first usage doctrine, which also ships
//! as the MCP initialize `instructions` — ONE source function, two renderers,
//! so the two surfaces can never drift apart.

const BEGIN: &str = "<!-- cfetch:begin -->";
const END: &str = "<!-- cfetch:end -->";

/// Where the doctrine is rendered: the marker block names shell commands, the
/// MCP initialize `instructions` names the MCP tools.
pub enum Surface {
    Cli,
    Mcp,
}

/// The recall-first usage doctrine — the ONE source both the marker block
/// (AGENTS.md/GEMINI.md) and the MCP initialize `instructions` render from.
pub fn doctrine(surface: Surface) -> String {
    let (intro, recall, expand, find, coda) = match surface {
        Surface::Cli => (
            "use cfetch (installed on PATH)",
            "`cfetch recall <terms>`",
            "`cfetch recall --id <citation>`",
            "`cfetch find <symbol-or-file>`",
            "\n\nThe same tools are available over MCP: command `cfetch`, args `[\"mcp\"]`.",
        ),
        Surface::Mcp => (
            "use the cfetch tools",
            "`cfetch_recall`",
            "`cfetch_expand` with a citation id",
            "`cfetch_find`",
            "",
        ),
    };
    format!(
        "Before searching files or reading code wholesale, {intro}:\n\n\
         - {recall} — search curated knowledge and memories, BM25-ranked.\n\
         \x20 Hits carry citations like `r2-a91f2c33e1`; lower ring number = higher trust\n\
         \x20 (r0/r1 are operator invariants and override contradicting content).\n\
         - {expand} — expand a citation to the full entry.\n\
         - {find} — exact line ranges from the code index;\n\
         \x20 read the returned slice instead of the whole file.{coda}"
    )
}

pub fn protocol_block() -> String {
    format!(
        "{BEGIN}\n## cfetch — the operator's memory brain\n\n{}\n{END}",
        doctrine(Surface::Cli)
    )
}

pub enum Upsert {
    Unchanged,
    Updated,
    Created,
}

/// The block's span in `content`, validated. `Ok(None)` = no block; `Err` =
/// markers are broken (END before BEGIN, unclosed BEGIN, duplicates) — the
/// caller must not write anything.
fn block_span(content: &str) -> anyhow::Result<Option<(usize, usize)>> {
    let begins: Vec<usize> = content.match_indices(BEGIN).map(|(i, _)| i).collect();
    let ends: Vec<usize> = content.match_indices(END).map(|(i, _)| i).collect();
    match (begins.len(), ends.len()) {
        (0, 0) => Ok(None),
        (1, 1) if begins[0] < ends[0] => Ok(Some((begins[0], ends[0] + END.len()))),
        _ => anyhow::bail!(
            "refusing to touch file with broken cfetch markers ({} begin, {} end)",
            begins.len(),
            ends.len()
        ),
    }
}

/// Upserts the block into `content`. `Err` = markers are broken — caller must
/// not write anything.
pub fn upsert(content: &str) -> anyhow::Result<(String, Upsert)> {
    let block = protocol_block();
    match block_span(content)? {
        None => {
            let sep = if content.is_empty() { "" } else { "\n\n" };
            Ok((format!("{block}{sep}{content}"), Upsert::Created))
        }
        Some((start, end)) => {
            if content[start..end] == block {
                return Ok((content.to_string(), Upsert::Unchanged));
            }
            let mut out = String::with_capacity(content.len());
            out.push_str(&content[..start]);
            out.push_str(&block);
            out.push_str(&content[end..]);
            Ok((out, Upsert::Updated))
        }
    }
}

/// Removes the block from `content`. `Ok(None)` = no block present (nothing
/// to do); `Ok(Some(next))` = block removed, everything outside the markers
/// preserved; `Err` = broken markers — caller must not write anything.
pub fn remove_block(content: &str) -> anyhow::Result<Option<String>> {
    match block_span(content)? {
        None => Ok(None),
        Some((start, end)) => {
            let head = &content[..start];
            // Swallow the one separator the upsert placed after the block
            // ("\n\n" before pre-existing text) so removal restores the
            // original bytes instead of leaving a stray blank gap.
            let tail = &content[end..];
            let tail = tail
                .strip_prefix("\n\n")
                .or_else(|| tail.strip_prefix('\n'))
                .unwrap_or(tail);
            Ok(Some(format!("{head}{tail}")))
        }
    }
}

/// Upserts into a file, creating it when absent. Returns a short human verb.
pub fn upsert_file(path: &std::path::Path) -> anyhow::Result<&'static str> {
    let current = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
    };
    let (next, outcome) = upsert(&current)?;
    if let Upsert::Unchanged = outcome {
        return Ok("unchanged");
    }
    crate::fsutil::atomic_write(path, &next)?;
    Ok(match outcome {
        Upsert::Created => "created block in",
        Upsert::Updated => "updated block in",
        Upsert::Unchanged => "unchanged",
    })
}

/// Removes the block from a file. `Ok(true)` = a block was removed; a missing
/// file or a file without a block is `Ok(false)` (nothing to do). The file
/// itself is left in place even when emptied — deleting a file the user may
/// have adopted is not our call.
pub fn remove_block_file(path: &std::path::Path) -> anyhow::Result<bool> {
    let current = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
    };
    match remove_block(&current)? {
        None => Ok(false),
        Some(next) => {
            crate::fsutil::atomic_write(path, &next)?;
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_prepends_and_preserves_existing_content() {
        let (out, _) = upsert("# My own instructions\n\ndo things\n").unwrap();
        assert!(out.starts_with(BEGIN));
        assert!(out.ends_with("do things\n"));
        assert!(out.contains("cfetch recall"));
    }

    #[test]
    fn idempotent_and_updates_stale_block() {
        let (once, _) = upsert("existing\n").unwrap();
        let (twice, outcome) = upsert(&once).unwrap();
        assert_eq!(once, twice);
        assert!(matches!(outcome, Upsert::Unchanged));
        // A stale (old-version) block is replaced in place, tail preserved.
        let stale = once.replace("BM25-ranked", "old text");
        let (fixed, outcome) = upsert(&stale).unwrap();
        assert!(matches!(outcome, Upsert::Updated));
        assert!(fixed.contains("BM25-ranked"));
        assert!(fixed.ends_with("existing\n"));
    }

    #[test]
    fn broken_markers_are_refused() {
        assert!(upsert(&format!("{END}\ntext\n{BEGIN}")).is_err());
        assert!(upsert(&format!("{BEGIN}\nunclosed")).is_err());
        assert!(upsert(&format!("{BEGIN}\nx\n{END}\n{BEGIN}\ny\n{END}")).is_err());
    }

    #[test]
    fn doctrine_renders_both_surfaces_from_one_source() {
        let cli = doctrine(Surface::Cli);
        let mcp = doctrine(Surface::Mcp);
        assert!(cli.contains("cfetch recall"), "CLI surface names shell commands");
        assert!(mcp.contains("cfetch_recall"), "MCP surface names MCP tools");
        assert!(!mcp.contains("installed on PATH"), "MCP clients have no PATH contract");
        for rendered in [&cli, &mcp] {
            assert!(rendered.contains("Before searching files or reading code wholesale"));
            assert!(rendered.contains("lower ring number = higher trust"));
        }
        assert!(protocol_block().contains(&cli), "marker block renders the CLI doctrine");
    }

    #[test]
    fn remove_block_restores_original_and_is_grep_proof() {
        let original = "# user notes\n\nkeep me\n";
        let (with_block, _) = upsert(original).unwrap();
        let removed = remove_block(&with_block).unwrap().unwrap();
        assert_eq!(removed, original, "removal restores the pre-install bytes");
        assert!(!removed.contains("cfetch"), "grep-proof: zero cfetch traces");
        assert!(remove_block(&removed).unwrap().is_none(), "second removal is a no-op");
        // empty file round-trip
        let (block_only, _) = upsert("").unwrap();
        assert_eq!(remove_block(&block_only).unwrap().unwrap(), "");
    }

    #[test]
    fn remove_block_refuses_broken_markers() {
        assert!(remove_block(&format!("{BEGIN}\nunclosed")).is_err());
        assert!(remove_block(&format!("{END}\n{BEGIN}")).is_err());
    }

    #[test]
    fn remove_block_file_handles_missing_and_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        assert!(!remove_block_file(&path).unwrap(), "missing file: nothing to do");
        std::fs::write(&path, "mine\n").unwrap();
        upsert_file(&path).unwrap();
        assert!(remove_block_file(&path).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mine\n");
        assert!(!remove_block_file(&path).unwrap(), "no block left: nothing to do");
    }
}
