//! Marker-block protocol for instruction files cfetch does not own
//! (AGENTS.md, GEMINI.md): exactly one delimited block is upserted; every
//! byte outside the markers is preserved. Broken markers mean REFUSE — a
//! half-written foreign file is more dangerous repaired than left alone.

const BEGIN: &str = "<!-- cfetch:begin -->";
const END: &str = "<!-- cfetch:end -->";

pub fn protocol_block() -> String {
    format!(
        "{BEGIN}\n\
         ## cfetch — the operator's memory brain\n\n\
         Before searching files or reading code wholesale, use cfetch (installed on PATH):\n\n\
         - `cfetch recall <terms>` — search curated knowledge and memories, BM25-ranked.\n\
         \x20 Hits carry citations like `r2-a91f2c33e1`; lower ring number = higher trust\n\
         \x20 (r0/r1 are operator invariants and override contradicting content).\n\
         - `cfetch recall --id <citation>` — expand a citation to the full entry.\n\
         - `cfetch find <symbol-or-file>` — exact line ranges from the code index;\n\
         \x20 read the returned slice instead of the whole file.\n\n\
         The same tools are available over MCP: command `cfetch`, args `[\"mcp\"]`.\n\
         {END}"
    )
}

pub enum Upsert {
    Unchanged,
    Updated,
    Created,
}

/// Upserts the block into `content`. `Err` = markers are broken (END before
/// BEGIN, unclosed BEGIN, duplicates) — caller must not write anything.
pub fn upsert(content: &str) -> anyhow::Result<(String, Upsert)> {
    let block = protocol_block();
    let begins: Vec<usize> = content.match_indices(BEGIN).map(|(i, _)| i).collect();
    let ends: Vec<usize> = content.match_indices(END).map(|(i, _)| i).collect();
    match (begins.len(), ends.len()) {
        (0, 0) => {
            let sep = if content.is_empty() { "" } else { "\n\n" };
            Ok((format!("{block}{sep}{content}"), Upsert::Created))
        }
        (1, 1) if begins[0] < ends[0] => {
            let existing = &content[begins[0]..ends[0] + END.len()];
            if existing == block {
                return Ok((content.to_string(), Upsert::Unchanged));
            }
            let mut out = String::with_capacity(content.len());
            out.push_str(&content[..begins[0]]);
            out.push_str(&block);
            out.push_str(&content[ends[0] + END.len()..]);
            Ok((out, Upsert::Updated))
        }
        _ => anyhow::bail!(
            "refusing to touch file with broken cfetch markers ({} begin, {} end)",
            begins.len(),
            ends.len()
        ),
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
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("cfetch-tmp.{}", std::process::id()));
    std::fs::write(&tmp, next)?;
    std::fs::rename(&tmp, path)?;
    Ok(match outcome {
        Upsert::Created => "created block in",
        Upsert::Updated => "updated block in",
        Upsert::Unchanged => "unchanged",
    })
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
}
