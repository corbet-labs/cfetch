//! Recall-first doctrine plus the marker-block guard for instruction files
//! cfetch does not own (AGENTS.md, GEMINI.md, CLAUDE.md, ...).
//!
//! New instruction placement is owned by `agent-config`; the old marker parser
//! remains only for the one-time conversion. Either way the delimiters are the
//! whole safety property, so cfetch validates them before anything is written
//! and refuses when they are broken.

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
    let (intro, recall, expand, find, maintain, coda) = match surface {
        Surface::Cli => (
            "use cfetch (installed on PATH)",
            "`cfetch recall <terms>`",
            "`cfetch recall --id <citation>`",
            "`cfetch find <symbol-or-file>`",
            "`cfetch maintain packet <candidate-id>`",
            "\n\nThe same tools are available over MCP: command `cfetch`, args `[\"mcp\"]`.",
        ),
        Surface::Mcp => (
            "use the cfetch tools",
            "`cfetch_recall`",
            "`cfetch_expand` with a citation id",
            "`cfetch_find`",
            "`cfetch_maintenance_packet`, `cfetch_maintenance_propose`, and `cfetch_maintenance_review`",
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
         \x20 read the returned slice instead of the whole file.\n\
         - {maintain} — inspect maintenance evidence and, when asked, place a typed proposal or review\n\
         \x20 in ring-5 quarantine. Never apply or finalize a memory change without explicit approval.{coda}"
    )
}

/// Every delimiter pair that can carry cfetch's block in a foreign file: the
/// one cfetch 0.9 wrote, the one `agent-config` writes today, and the
/// hook-prefixed form that pre-rename `agent-config` installs left behind.
fn fences(instruction_name: &str) -> [(String, String); 3] {
    [
        (BEGIN.to_string(), END.to_string()),
        (
            format!("<!-- BEGIN AGENT-CONFIG-INSTR:{instruction_name} -->"),
            format!("<!-- END AGENT-CONFIG-INSTR:{instruction_name} -->"),
        ),
        (
            format!("<!-- BEGIN AGENT-CONFIG:{instruction_name} -->"),
            format!("<!-- END AGENT-CONFIG:{instruction_name} -->"),
        ),
    ]
}

/// What is wrong with one fence in `content`, phrased for whoever has to open
/// the file and repair it by hand. `None` = the fence is absent, or forms
/// exactly one well-ordered block.
fn fence_fault(content: &str, begin: &str, end: &str) -> Option<String> {
    let begins: Vec<usize> = content.match_indices(begin).map(|(i, _)| i).collect();
    let ends: Vec<usize> = content.match_indices(end).map(|(i, _)| i).collect();
    match (begins.as_slice(), ends.as_slice()) {
        ([], []) => None,
        ([opened], [closed]) if opened < closed => None,
        ([_], [_]) => Some(format!("{end} appears before {begin}")),
        ([_], []) => Some(format!("{begin} is never closed by {end}")),
        ([], [_]) => Some(format!("{end} has no matching {begin}")),
        _ => Some(format!(
            "{} {begin} and {} {end} markers; exactly one of each is expected",
            begins.len(),
            ends.len()
        )),
    }
}

/// Refuses when `path` already carries a cfetch block whose delimiters cannot
/// be read unambiguously. `agent-config`'s instruction upsert replaces the
/// first BEGIN..END it can find and appends a fresh block when it finds none,
/// so a duplicated, unclosed or inverted fence is written over rather than
/// reported — in a file cfetch does not own and every session loads. Which
/// half of a broken fence is ours is not knowable, so the only safe move is to
/// touch nothing and name both the file and the fault.
pub fn ensure_no_broken_block(
    path: &std::path::Path,
    instruction_name: &str,
) -> anyhow::Result<()> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(anyhow::anyhow!("read {}: {error}", path.display())),
    };
    let faults: Vec<String> = fences(instruction_name)
        .iter()
        .filter_map(|(begin, end)| fence_fault(&content, begin, end))
        .collect();
    if faults.is_empty() {
        return Ok(());
    }
    anyhow::bail!("refusing to write {}: {}", path.display(), faults.join("; "))
}

/// The block's span in `content`, validated. `Ok(None)` = no block; `Err` =
/// markers are broken (END before BEGIN, unclosed BEGIN, duplicates) — the
/// caller must not write anything.
fn block_span(content: &str) -> anyhow::Result<Option<(usize, usize)>> {
    if let Some(fault) = fence_fault(content, BEGIN, END) {
        anyhow::bail!("broken cfetch markers: {fault}");
    }
    let (Some(begin), Some(end)) = (content.find(BEGIN), content.find(END)) else {
        return Ok(None);
    };
    Ok(Some((begin, end + END.len())))
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
    let removed = remove_block(&current)
        .map_err(|error| anyhow::anyhow!("refusing to touch {}: {error}", path.display()))?;
    match removed {
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

    fn legacy_block(tail: &str) -> String {
        let separator = if tail.is_empty() { "" } else { "\n\n" };
        format!("{BEGIN}\nold cfetch instructions\n{END}{separator}{tail}")
    }

    // The fence agent-config writes into AGENTS.md for cfetch today.
    const OPEN: &str = "<!-- BEGIN AGENT-CONFIG-INSTR:CFETCH -->";
    const CLOSE: &str = "<!-- END AGENT-CONFIG-INSTR:CFETCH -->";

    fn instruction_block(body: &str) -> String {
        format!("{OPEN}\n{body}\n{CLOSE}\n")
    }

    fn refusal(content: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, content).unwrap();
        let error = ensure_no_broken_block(&path, "CFETCH")
            .expect_err("a broken fence must refuse")
            .to_string();
        assert!(error.contains(&path.display().to_string()), "names the file: {error}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            content,
            "a refusal leaves the foreign file byte-identical"
        );
        error
    }

    #[test]
    fn broken_markers_are_refused() {
        assert!(remove_block(&format!("{END}\ntext\n{BEGIN}")).is_err());
        assert!(remove_block(&format!("{BEGIN}\nunclosed")).is_err());
        assert!(remove_block(&format!("{BEGIN}\nx\n{END}\n{BEGIN}\ny\n{END}")).is_err());
    }

    #[test]
    fn every_malformed_shape_is_named_and_refused() {
        let unclosed = refusal(&format!("# notes\n\n{OPEN}\nbody\n"));
        assert!(unclosed.contains("never closed"), "{unclosed}");
        let orphan_close = refusal(&format!("# notes\n\n{CLOSE}\n"));
        assert!(orphan_close.contains("no matching"), "{orphan_close}");
        let inverted = refusal(&format!("{CLOSE}\nbody\n{OPEN}\n"));
        assert!(inverted.contains("appears before"), "{inverted}");
        let duplicated = refusal(&format!(
            "{}{}",
            instruction_block("one"),
            instruction_block("two")
        ));
        assert!(duplicated.contains("exactly one of each"), "{duplicated}");

        // The 0.9 fence and the pre-rename hook-prefixed fence are upserted by
        // the same code paths, so they get the same guard.
        let legacy_09 = refusal(&format!("{BEGIN}\nold\n"));
        assert!(legacy_09.contains(BEGIN) && legacy_09.contains("never closed"), "{legacy_09}");
        let pre_rename = refusal("<!-- BEGIN AGENT-CONFIG:CFETCH -->\nold\n");
        assert!(pre_rename.contains("AGENT-CONFIG:CFETCH"), "{pre_rename}");
    }

    #[test]
    fn well_formed_and_absent_blocks_are_writable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        ensure_no_broken_block(&path, "CFETCH").expect("a file that does not exist yet is fine");
        for content in [
            "# my own notes\n".to_string(),
            instruction_block("doctrine"),
            format!("# notes\n\n{}", instruction_block("doctrine")),
            legacy_block("# notes\n"),
        ] {
            std::fs::write(&path, &content).unwrap();
            ensure_no_broken_block(&path, "CFETCH")
                .unwrap_or_else(|error| panic!("well-formed content refused: {error}"));
        }
        // A foreign harness's block under a different name is none of our
        // business, broken or not.
        std::fs::write(&path, "<!-- BEGIN AGENT-CONFIG-INSTR:OTHER -->\n").unwrap();
        ensure_no_broken_block(&path, "CFETCH").unwrap();
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
    }

    #[test]
    fn remove_block_restores_original_and_is_grep_proof() {
        let original = "# user notes\n\nkeep me\n";
        let with_block = legacy_block(original);
        let removed = remove_block(&with_block).unwrap().unwrap();
        assert_eq!(removed, original, "removal restores the pre-install bytes");
        assert!(!removed.contains("cfetch"), "grep-proof: zero cfetch traces");
        assert!(remove_block(&removed).unwrap().is_none(), "second removal is a no-op");
        // empty file round-trip
        let block_only = legacy_block("");
        assert_eq!(remove_block(&block_only).unwrap().unwrap(), "");
    }

    #[test]
    fn remove_block_refuses_broken_markers() {
        assert!(remove_block(&format!("{BEGIN}\nunclosed")).is_err());
        assert!(remove_block(&format!("{END}\n{BEGIN}")).is_err());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        let broken = format!("{BEGIN}\nunclosed\n");
        std::fs::write(&path, &broken).unwrap();
        let error = remove_block_file(&path).unwrap_err().to_string();
        assert!(error.contains(&path.display().to_string()), "{error}");
        assert!(error.contains("never closed"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            broken,
            "a refusal leaves the foreign file byte-identical"
        );
    }

    #[test]
    fn remove_block_file_handles_missing_and_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        assert!(!remove_block_file(&path).unwrap(), "missing file: nothing to do");
        std::fs::write(&path, legacy_block("mine\n")).unwrap();
        assert!(remove_block_file(&path).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mine\n");
        assert!(!remove_block_file(&path).unwrap(), "no block left: nothing to do");
    }
}
