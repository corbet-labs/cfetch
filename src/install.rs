//! Registers cfetch's hooks in Claude Code and Codex using the
//! managed-entry merge: only entries tagged `_managedBy: "cfetch"` are ever
//! removed or replaced; everything else in the file — including a user hook
//! that happens to invoke the same command — is preserved byte-for-byte at the
//! JSON level.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::paths;

const MANAGED_BY: &str = "cfetch";

/// (harness event key, cfetch hook subcommand)
const EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "user-prompt"),
    ("PreToolUse", "pre-tool"),
    ("PostToolUse", "post-tool"),
    ("Stop", "stop"),
    ("PreCompact", "precompact"),
];

pub fn default_settings_path() -> PathBuf {
    paths::home().join(".claude/settings.json")
}

/// POSIX shell quoting: single-quote the whole word, and close/escape/reopen
/// around any embedded single quote.
fn posix_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// Windows command-processor quoting: double-quote the whole word.
///
/// `cmd.exe` treats an apostrophe as an ordinary character, so the POSIX form
/// would make the harness look for a program literally named `'C:\...` and
/// every hook would silently never run. A double quote cannot appear in a
/// Windows path (it is an illegal filename character), so there is nothing
/// left to escape. `%` is legal in a path and would still be read as an
/// environment reference by `cmd.exe`; that is inherent to the command
/// processor and out of reach of quoting.
fn windows_quote(word: &str) -> String {
    format!("\"{word}\"")
}

/// Quoting for THIS platform's command processor.
//
// The branch is a runtime `cfg!`, not a `#[cfg]`: both quoters stay compiled
// on every platform, so the tests below prove both on any runner.
fn shell_quote(word: &str) -> String {
    if cfg!(windows) { windows_quote(word) } else { posix_quote(word) }
}

/// The command embeds the absolute binary path: hooks run outside a login
/// shell, so PATH is not a contract. Quoted for the platform's command
/// processor, against spaces in the path.
fn hook_command_for(exe: &str, subcommand: &str) -> String {
    format!("{} hook {subcommand}", shell_quote(exe))
}

#[cfg(test)]
fn hook_command(subcommand: &str) -> String {
    hook_command_for(&current_exe_str(), subcommand)
}

fn managed_entry_for(exe: &str, subcommand: &str) -> Value {
    json!({
        "hooks": [{
            "type": "command",
            "command": hook_command_for(exe, subcommand),
            "timeout": 10,
            "_managedBy": MANAGED_BY,
        }]
    })
}

/// Removes OUR tagged hook objects from an entry's inner `hooks` array —
/// never the whole entry, which may co-locate user hooks. Returns whether the
/// entry still does anything and should be kept.
fn strip_managed(entry: &mut Value) -> bool {
    match entry.get_mut("hooks").and_then(Value::as_array_mut) {
        Some(hooks) => {
            hooks.retain(|h| h.get("_managedBy").and_then(Value::as_str) != Some(MANAGED_BY));
            !hooks.is_empty()
        }
        None => true, // not a shape we own; leave it alone
    }
}

/// Pure merge with an explicit executable so both agent installers and tests
/// can key registration to the same binary path.
fn merge_for_exe(settings: Value, exe: &str) -> anyhow::Result<Value> {
    let mut root = match settings {
        Value::Object(m) => m,
        Value::Null => Map::new(),
        other => anyhow::bail!("settings.json is not a JSON object (found {other})"),
    };
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings.json 'hooks' is not an object"))?;

    for (event_key, subcommand) in EVENTS {
        let list = hooks
            .entry(event_key.to_string())
            .or_insert_with(|| Value::Array(vec![]));
        let list = list
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("settings.json hooks.{event_key} is not an array"))?;
        list.retain_mut(strip_managed);
        list.push(managed_entry_for(exe, subcommand));
    }
    Ok(Value::Object(root))
}

/// Pure merge so it is testable: returns the new settings document.
pub fn merge(settings: Value) -> anyhow::Result<Value> {
    merge_for_exe(settings, &current_exe_str())
}

/// Removes every managed entry (uninstall). Leaves empty arrays in place —
/// removing structure the user may have had reasons for is not our call.
pub fn unmerge(settings: Value) -> anyhow::Result<Value> {
    let mut root = match settings {
        Value::Object(m) => m,
        other => return Ok(other),
    };
    if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
        for (_, list) in hooks.iter_mut() {
            if let Some(arr) = list.as_array_mut() {
                arr.retain_mut(strip_managed);
            }
        }
    }
    Ok(Value::Object(root))
}

/// Whether a TOML mcp-server entry already carries the CURRENT command/args.
fn codex_entry_is_current(entry: &toml_edit::Item, exe: &str) -> bool {
    let Some(t) = entry.as_table_like() else { return false };
    t.get("command").and_then(|v| v.as_str()) == Some(exe)
        && t.get("args")
            .and_then(|v| v.as_array())
            .is_some_and(|a| a.len() == 1 && a.get(0).and_then(|x| x.as_str()) == Some("mcp"))
}

/// Structured upsert of the cfetch MCP server into Codex's config.toml,
/// via toml_edit so the user's comments and formatting survive byte-for-byte.
/// An unparseable file is refused outright (like the Gemini JSON path) —
/// appending to a file we cannot parse could corrupt it. Content-keyed:
/// `Ok(None)` = entry already carries the current command/args; a stale entry
/// (the binary moved) is repaired in place, extra user keys preserved.
fn codex_toml_with_mcp(content: &str, exe: &str) -> anyhow::Result<Option<String>> {
    let mut doc: toml_edit::DocumentMut = content
        .parse()
        .map_err(|e| anyhow::anyhow!("refusing to touch unparseable TOML: {e}"))?;
    let already_current = doc
        .get("mcp_servers")
        .and_then(|s| s.as_table_like())
        .and_then(|s| s.get("cfetch"))
        .is_some_and(|entry| codex_entry_is_current(entry, exe));
    if already_current {
        return Ok(None);
    }
    if doc.get("mcp_servers").is_none() {
        let mut t = toml_edit::Table::new();
        // Implicit: renders only `[mcp_servers.cfetch]`, no bare header line.
        t.set_implicit(true);
        doc.insert("mcp_servers", toml_edit::Item::Table(t));
    }
    let servers = doc["mcp_servers"]
        .as_table_like_mut()
        .ok_or_else(|| anyhow::anyhow!("config.toml `mcp_servers` is not a table"))?;
    if !servers.get("cfetch").is_some_and(|e| e.as_table_like().is_some()) {
        servers.insert("cfetch", toml_edit::Item::Table(toml_edit::Table::new()));
    }
    let entry = servers
        .get_mut("cfetch")
        .and_then(|e| e.as_table_like_mut())
        .expect("just ensured a table-like cfetch entry");
    entry.insert("command", toml_edit::value(exe));
    let mut args = toml_edit::Array::new();
    args.push("mcp");
    entry.insert("args", toml_edit::value(args));
    Ok(Some(doc.to_string()))
}

/// Removes the cfetch MCP server from Codex's config.toml. `Ok(None)` =
/// nothing of ours present. Unparseable = refuse, same as the upsert.
fn codex_toml_without_mcp(content: &str) -> anyhow::Result<Option<String>> {
    let mut doc: toml_edit::DocumentMut = content
        .parse()
        .map_err(|e| anyhow::anyhow!("refusing to touch unparseable TOML: {e}"))?;
    let removed = doc
        .get_mut("mcp_servers")
        .and_then(|s| s.as_table_like_mut())
        .and_then(|s| s.remove("cfetch"))
        .is_some();
    if !removed {
        return Ok(None);
    }
    Ok(Some(doc.to_string()))
}

/// Content-keyed merge of the cfetch MCP server into Gemini's settings.json:
/// `Ok(None)` = entry already carries the current command/args; a stale entry
/// is repaired in place, extra user keys inside it preserved.
fn gemini_settings_with_mcp(settings: Value, exe: &str) -> anyhow::Result<Option<Value>> {
    let mut root = match settings {
        Value::Object(m) => m,
        Value::Null => Map::new(),
        other => anyhow::bail!("settings.json is not a JSON object (found {other})"),
    };
    let servers = root
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()));
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings.json 'mcpServers' is not an object"))?;
    let desired_command = Value::String(exe.to_string());
    let desired_args = json!(["mcp"]);
    if let Some(current) = servers.get("cfetch").and_then(Value::as_object)
        && current.get("command") == Some(&desired_command)
        && current.get("args") == Some(&desired_args)
    {
        return Ok(None);
    }
    let entry = servers.entry("cfetch").or_insert_with(|| json!({}));
    if !entry.is_object() {
        *entry = json!({});
    }
    let entry = entry.as_object_mut().expect("just ensured an object");
    entry.insert("command".into(), desired_command);
    entry.insert("args".into(), desired_args);
    Ok(Some(Value::Object(root)))
}

/// Removes the cfetch entry from Gemini's settings.json; `None` = nothing of
/// ours present (including a non-object document: nothing we wrote survives
/// in a shape we did not write).
fn gemini_settings_without_mcp(settings: Value) -> Option<Value> {
    let Value::Object(mut root) = settings else { return None };
    root.get_mut("mcpServers")?.as_object_mut()?.remove("cfetch")?;
    Some(Value::Object(root))
}

fn current_exe_str() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "cfetch".to_string())
}

fn json_file(path: &Path) -> anyhow::Result<Value> {
    let raw = read_or_empty(path)?;
    if raw.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("refusing to touch unparseable {}: {e}", path.display()))
}

fn hooks_are_current(settings: &Value, exe: &str) -> bool {
    let Some(hooks) = settings.get("hooks").and_then(Value::as_object) else {
        return false;
    };
    EVENTS.iter().all(|(event, subcommand)| {
        let expected = hook_command_for(exe, subcommand);
        hooks
            .get(*event)
            .and_then(Value::as_array)
            .is_some_and(|groups| {
                groups.iter().any(|group| {
                    group
                        .get("hooks")
                        .and_then(Value::as_array)
                        .is_some_and(|handlers| {
                            handlers.iter().any(|handler| {
                                handler.get("_managedBy").and_then(Value::as_str)
                                    == Some(MANAGED_BY)
                                    && handler.get("command").and_then(Value::as_str)
                                        == Some(expected.as_str())
                            })
                        })
                })
            })
    })
}

/// Reports drift in a detected Codex installation. `None` means Codex is not
/// installed; an empty list means AGENTS.md, native hooks, and MCP all point
/// at this executable. This is deliberately read-only for `selfcheck`.
pub fn codex_registration_issues() -> Option<Vec<String>> {
    codex_registration_issues_at(&paths::home(), &current_exe_str())
}

fn codex_registration_issues_at(home: &Path, exe: &str) -> Option<Vec<String>> {
    let codex = home.join(".codex");
    if !codex.is_dir() {
        return None;
    }
    let mut issues = Vec::new();

    let agents_md = codex.join("AGENTS.md");
    match std::fs::read_to_string(&agents_md) {
        Ok(content) => match crate::markers::upsert(&content) {
            Ok((next, _)) if next == content => {}
            Ok(_) => issues.push(format!("{} lacks the current cfetch block", agents_md.display())),
            Err(e) => issues.push(format!("{}: {e}", agents_md.display())),
        },
        Err(e) => issues.push(format!("read {}: {e}", agents_md.display())),
    }

    let hooks_path = codex.join("hooks.json");
    match json_file(&hooks_path) {
        Ok(settings) if hooks_are_current(&settings, exe) => {}
        Ok(_) => issues.push(format!(
            "{} lacks current cfetch native hooks",
            hooks_path.display()
        )),
        Err(e) => issues.push(e.to_string()),
    }

    let toml_path = codex.join("config.toml");
    match read_or_empty(&toml_path).and_then(|content| {
        content
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| anyhow::anyhow!("parse {}: {e}", toml_path.display()))
    }) {
        Ok(doc)
            if doc
                .get("mcp_servers")
                .and_then(|servers| servers.as_table_like())
                .and_then(|servers| servers.get("cfetch"))
                .is_some_and(|entry| codex_entry_is_current(entry, exe)) => {}
        Ok(_) => issues.push(format!(
            "{} has no current cfetch MCP command",
            toml_path.display()
        )),
        Err(e) => issues.push(e.to_string()),
    }
    Some(issues)
}

/// Reads a file that may legitimately not exist yet. Only NotFound maps to
/// empty — an unreadable existing file must never be treated as absent and
/// then overwritten.
fn read_or_empty(path: &Path) -> anyhow::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(anyhow::anyhow!("read {}: {e}", path.display())),
    }
}

/// Atomic replace: tmp file in the same directory + rename.
fn write_atomic(path: &Path, content: &str) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("cfetch-tmp.{}", std::process::id()));
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Registers cfetch with every other agent found on this machine —
/// feature-detected, instruction blocks + MCP, nothing is created for agents
/// that are not installed. Re-running repairs drift: a registration whose
/// embedded binary path went stale is updated in place.
pub fn install_agents() -> anyhow::Result<()> {
    let exe = current_exe_str();
    let codex = paths::home().join(".codex");
    if codex.is_dir() {
        let agents_md = codex.join("AGENTS.md");
        let verb = crate::markers::upsert_file(&agents_md)?;
        println!("codex: {verb} {}", agents_md.display());
        let toml_path = codex.join("config.toml");
        let current = read_or_empty(&toml_path)?;
        if let Some(next) = codex_toml_with_mcp(&current, &exe)
            .map_err(|e| anyhow::anyhow!("{}: {e}", toml_path.display()))?
        {
            write_atomic(&toml_path, &next)?;
            println!("codex: registered MCP server in {}", toml_path.display());
        }
        let hooks_path = codex.join("hooks.json");
        let current = json_file(&hooks_path)?;
        let next = merge_for_exe(current.clone(), &exe)?;
        if next != current {
            write_atomic(&hooks_path, &serde_json::to_string_pretty(&next)?)?;
            println!(
                "codex: registered native hooks in {} (approve once with /hooks)",
                hooks_path.display()
            );
        }
    }
    let gemini = paths::home().join(".gemini");
    if gemini.is_dir() {
        let gemini_md = gemini.join("GEMINI.md");
        let verb = crate::markers::upsert_file(&gemini_md)?;
        println!("gemini: {verb} {}", gemini_md.display());
        let settings_path = gemini.join("settings.json");
        let raw = read_or_empty(&settings_path)?;
        let current: Value = if raw.trim().is_empty() {
            Value::Object(Map::new())
        } else {
            serde_json::from_str(&raw).map_err(|e| {
                anyhow::anyhow!("refusing to touch unparseable {}: {e}", settings_path.display())
            })?
        };
        if let Some(next) = gemini_settings_with_mcp(current, &exe)? {
            write_atomic(&settings_path, &serde_json::to_string_pretty(&next)?)?;
            println!("gemini: registered MCP server in {}", settings_path.display());
        }
    }
    Ok(())
}

/// Symmetric uninstall: removes exactly what install_agents() creates — the
/// AGENTS.md/GEMINI.md marker blocks, Codex native hooks and
/// `mcp_servers.cfetch`, and the Gemini `mcpServers.cfetch` entry.
/// Feature-detected the same way; everything the user wrote stays.
pub fn uninstall_agents() -> anyhow::Result<()> {
    let codex = paths::home().join(".codex");
    if codex.is_dir() {
        let agents_md = codex.join("AGENTS.md");
        if crate::markers::remove_block_file(&agents_md)? {
            println!("codex: removed block from {}", agents_md.display());
        }
        let toml_path = codex.join("config.toml");
        if toml_path.is_file() {
            let current = read_or_empty(&toml_path)?;
            if let Some(next) = codex_toml_without_mcp(&current)
                .map_err(|e| anyhow::anyhow!("{}: {e}", toml_path.display()))?
            {
                write_atomic(&toml_path, &next)?;
                println!("codex: removed MCP server from {}", toml_path.display());
            }
        }
        let hooks_path = codex.join("hooks.json");
        if hooks_path.is_file() {
            let current = json_file(&hooks_path)?;
            let next = unmerge(current.clone())?;
            if next != current {
                write_atomic(&hooks_path, &serde_json::to_string_pretty(&next)?)?;
                println!("codex: removed native hooks from {}", hooks_path.display());
            }
        }
    }
    let gemini = paths::home().join(".gemini");
    if gemini.is_dir() {
        let gemini_md = gemini.join("GEMINI.md");
        if crate::markers::remove_block_file(&gemini_md)? {
            println!("gemini: removed block from {}", gemini_md.display());
        }
        let settings_path = gemini.join("settings.json");
        if settings_path.is_file() {
            let raw = read_or_empty(&settings_path)?;
            let current: Value = serde_json::from_str(&raw).map_err(|e| {
                anyhow::anyhow!("refusing to touch unparseable {}: {e}", settings_path.display())
            })?;
            if let Some(next) = gemini_settings_without_mcp(current) {
                write_atomic(&settings_path, &serde_json::to_string_pretty(&next)?)?;
                println!("gemini: removed MCP server from {}", settings_path.display());
            }
        }
    }
    Ok(())
}

pub fn apply(settings_path: &Path, remove: bool) -> anyhow::Result<()> {
    let current: Value = match std::fs::read_to_string(settings_path) {
        Ok(s) => serde_json::from_str(&s)
            .map_err(|e| anyhow::anyhow!("refusing to touch unparseable {}: {e}", settings_path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Object(Map::new()),
        Err(e) => return Err(anyhow::anyhow!("read {}: {e}", settings_path.display())),
    };
    let next = if remove { unmerge(current)? } else { merge(current)? };
    if let Some(dir) = settings_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let rendered = serde_json::to_string_pretty(&next)?;
    let tmp = settings_path.with_extension("json.cfetch-tmp");
    std::fs::write(&tmp, rendered)?;
    std::fs::rename(&tmp, settings_path)?;
    println!(
        "{} cfetch hooks in {}",
        if remove { "removed" } else { "registered" },
        settings_path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_into_empty_registers_all_events() {
        let out = merge(Value::Null).unwrap();
        let hooks = out["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), EVENTS.len());
        assert!(hooks["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("session-start"));
    }

    #[test]
    fn codex_hook_document_uses_the_native_schema_and_absolute_commands() {
        let out = merge_for_exe(Value::Null, "/opt/cfetch/bin/cfetch").unwrap();
        assert!(out.get("hooks").is_some(), "Codex requires the hooks wrapper");
        for (event, subcommand) in EVENTS {
            let handler = &out["hooks"][event][0]["hooks"][0];
            assert_eq!(handler["type"], "command");
            let command = handler["command"].as_str().unwrap();
            assert!(command.contains("/opt/cfetch/bin/cfetch"));
            assert!(command.ends_with(&format!(" hook {subcommand}")));
        }
    }

    #[test]
    fn user_prompt_submit_hook_is_registered() {
        let out = merge(Value::Null).unwrap();
        let entry = &out["hooks"]["UserPromptSubmit"][0]["hooks"][0];
        assert!(entry["command"].as_str().unwrap().contains("hook user-prompt"));
        assert_eq!(entry["timeout"], 10);
        assert_eq!(entry["_managedBy"], MANAGED_BY);
    }

    #[test]
    fn merge_preserves_foreign_entries_and_is_idempotent() {
        let existing = json!({
            "permissions": {"allow": ["Bash(ls:*)"]},
            "hooks": {
                "SessionStart": [
                    {"hooks": [{"type": "command", "command": "my-own-hook"}]}
                ]
            }
        });
        let once = merge(existing).unwrap();
        let twice = merge(once.clone()).unwrap();
        assert_eq!(once, twice, "merge must be idempotent");
        let list = twice["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["hooks"][0]["command"], "my-own-hook");
        assert_eq!(twice["permissions"]["allow"][0], "Bash(ls:*)");
    }

    #[test]
    fn foreign_entry_with_same_command_is_not_ours() {
        // A user hook invoking the identical command but without the tag must
        // never be removed.
        let existing = json!({
            "hooks": {"Stop": [
                {"hooks": [{"type": "command", "command": "cfetch hook stop"}]}
            ]}
        });
        let out = merge(existing).unwrap();
        let list = out["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn user_hook_colocated_in_managed_entry_survives() {
        // A user may append their own hook object INTO our managed entry's
        // hooks array; merge/unmerge must remove only the tagged object.
        let mut merged = merge(Value::Null).unwrap();
        merged["hooks"]["Stop"][0]["hooks"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "command", "command": "user-added-inside"}));
        let remerged = merge(merged.clone()).unwrap();
        let all: String = serde_json::to_string(&remerged).unwrap();
        assert!(all.contains("user-added-inside"), "co-located user hook was deleted");
        let unmerged = unmerge(remerged).unwrap();
        let s = serde_json::to_string(&unmerged).unwrap();
        assert!(s.contains("user-added-inside"));
        assert!(!s.contains(MANAGED_BY));
    }

    #[test]
    fn codex_toml_upsert_preserves_comments_and_is_idempotent() {
        let input = "# my codex config\nmodel = \"o5\" # pinned on purpose\n\n[profiles.fast]\nmodel = \"o5-mini\"\n";
        let once = codex_toml_with_mcp(input, "/usr/bin/cfetch").unwrap().unwrap();
        assert!(once.starts_with(input), "user bytes (comments included) preserved verbatim");
        assert!(once.contains("[mcp_servers.cfetch]"));
        assert!(once.contains("command = \"/usr/bin/cfetch\""));
        assert!(once.contains("args = [\"mcp\"]"));
        assert!(
            codex_toml_with_mcp(&once, "/usr/bin/cfetch").unwrap().is_none(),
            "current registration: no rewrite"
        );
        // empty file (fresh install): just our table
        let fresh = codex_toml_with_mcp("", "/usr/bin/cfetch").unwrap().unwrap();
        assert!(fresh.trim_start().starts_with("[mcp_servers.cfetch]"), "no bare [mcp_servers] header:\n{fresh}");
    }

    #[test]
    fn codex_toml_parse_error_is_refused() {
        assert!(codex_toml_with_mcp("model = \"unclosed", "/x").is_err());
        assert!(codex_toml_without_mcp("model = \"unclosed").is_err());
    }

    #[test]
    fn codex_toml_stale_path_is_repaired_preserving_user_keys() {
        // Binary moved (e.g. package update): the registration must follow.
        let stale = "# note\n[mcp_servers.cfetch]\ncommand = \"/old/place/cfetch\"\nargs = [\"mcp\"]\nstartup_timeout_ms = 9000\n";
        let out = codex_toml_with_mcp(stale, "/new/place/cfetch").unwrap().unwrap();
        assert!(out.contains("command = \"/new/place/cfetch\""));
        assert!(!out.contains("/old/place"), "stale path gone");
        assert!(out.contains("startup_timeout_ms = 9000"), "user-added keys survive the repair");
        assert!(out.contains("# note"));
        assert!(codex_toml_with_mcp(&out, "/new/place/cfetch").unwrap().is_none());
    }

    #[test]
    fn codex_toml_removal_is_grep_proof_and_leaves_others() {
        let input = "# keep this comment\n[mcp_servers.other]\ncommand = \"x\"\n\n[mcp_servers.cfetch]\ncommand = \"/usr/bin/cfetch\"\nargs = [\"mcp\"]\n";
        let out = codex_toml_without_mcp(input).unwrap().unwrap();
        assert!(!out.contains("cfetch"), "grep-proof: zero cfetch traces, got:\n{out}");
        assert!(out.contains("[mcp_servers.other]"), "foreign server survives");
        assert!(out.contains("# keep this comment"));
        assert!(codex_toml_without_mcp(&out).unwrap().is_none(), "second removal is a no-op");
        assert!(codex_toml_without_mcp("model = \"o5\"\n").unwrap().is_none(), "nothing of ours: no rewrite");
    }

    #[test]
    fn codex_registration_check_covers_doctrine_hooks_and_mcp() {
        let home = tempfile::tempdir().unwrap();
        let codex = home.path().join(".codex");
        std::fs::create_dir(&codex).unwrap();
        let exe = "/usr/bin/cfetch";

        let initial = codex_registration_issues_at(home.path(), exe).unwrap();
        assert_eq!(initial.len(), 3, "all three Codex surfaces are absent");

        let (agents, _) = crate::markers::upsert("# user instructions\n").unwrap();
        std::fs::write(codex.join("AGENTS.md"), agents).unwrap();
        let hooks = merge_for_exe(Value::Null, exe).unwrap();
        std::fs::write(codex.join("hooks.json"), serde_json::to_string(&hooks).unwrap()).unwrap();
        let config = codex_toml_with_mcp("model = \"gpt-test\"\n", exe).unwrap().unwrap();
        std::fs::write(codex.join("config.toml"), config).unwrap();

        assert!(
            codex_registration_issues_at(home.path(), exe).unwrap().is_empty(),
            "fully registered Codex installation is current"
        );

        let stale_hooks = merge_for_exe(Value::Null, "/retired/cfetch").unwrap();
        std::fs::write(
            codex.join("hooks.json"),
            serde_json::to_string(&stale_hooks).unwrap(),
        )
        .unwrap();
        let issues = codex_registration_issues_at(home.path(), exe).unwrap();
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("native hooks"));
    }

    #[test]
    fn gemini_merge_preserves_and_is_idempotent() {
        let existing = json!({"theme": "dark", "mcpServers": {"other": {"command": "x"}}});
        let merged = gemini_settings_with_mcp(existing, "/usr/bin/cfetch").unwrap().unwrap();
        assert_eq!(merged["theme"], "dark");
        assert_eq!(merged["mcpServers"]["other"]["command"], "x");
        assert_eq!(merged["mcpServers"]["cfetch"]["args"][0], "mcp");
        assert!(gemini_settings_with_mcp(merged, "/usr/bin/cfetch").unwrap().is_none());
    }

    #[test]
    fn gemini_stale_path_is_repaired_preserving_user_keys() {
        let stale = json!({"mcpServers": {"cfetch": {
            "command": "/old/place/cfetch", "args": ["mcp"], "timeout": 30000
        }}});
        let out = gemini_settings_with_mcp(stale, "/new/place/cfetch").unwrap().unwrap();
        assert_eq!(out["mcpServers"]["cfetch"]["command"], "/new/place/cfetch");
        assert_eq!(out["mcpServers"]["cfetch"]["timeout"], 30000, "user-added keys survive");
        assert!(gemini_settings_with_mcp(out, "/new/place/cfetch").unwrap().is_none());
    }

    #[test]
    fn gemini_removal_is_grep_proof_and_leaves_others() {
        let v = json!({"theme": "dark", "mcpServers": {
            "other": {"command": "x"},
            "cfetch": {"command": "/usr/bin/cfetch", "args": ["mcp"]}
        }});
        let out = gemini_settings_without_mcp(v).unwrap();
        let s = serde_json::to_string(&out).unwrap();
        assert!(!s.contains("cfetch"), "grep-proof: zero cfetch traces");
        assert_eq!(out["mcpServers"]["other"]["command"], "x");
        assert_eq!(out["theme"], "dark");
        assert!(gemini_settings_without_mcp(out).is_none(), "second removal is a no-op");
        assert!(gemini_settings_without_mcp(json!({"theme": "dark"})).is_none());
    }

    #[test]
    fn posix_quoting_wraps_and_escapes_apostrophes() {
        assert_eq!(posix_quote("/usr/bin/cfetch"), "'/usr/bin/cfetch'");
        assert_eq!(posix_quote("/opt/a b/cfetch"), "'/opt/a b/cfetch'");
        assert_eq!(posix_quote("/o'brien/cfetch"), r"'/o'\''brien/cfetch'");
    }

    #[test]
    fn windows_quoting_double_quotes_the_path() {
        // The POSIX form is not merely ugly on cmd.exe, it is broken: the
        // apostrophes become part of the program name.
        assert_eq!(
            windows_quote(r"C:\Program Files\cfetch\cfetch.exe"),
            "\"C:\\Program Files\\cfetch\\cfetch.exe\""
        );
        assert!(!windows_quote(r"C:\x\cfetch.exe").contains('\''), "no POSIX quoting on Windows");
    }

    #[test]
    fn hook_command_uses_this_platforms_quoting() {
        let cmd = hook_command("session-start");
        let quoted = cmd
            .strip_suffix(" hook session-start")
            .unwrap_or_else(|| panic!("unexpected command shape: {cmd}"));
        let (open, close) = if cfg!(windows) { ('"', '"') } else { ('\'', '\'') };
        assert!(quoted.starts_with(open), "{cmd}");
        assert!(quoted.ends_with(close), "{cmd}");
    }

    #[test]
    fn registered_commands_are_quoted_for_this_platform() {
        let out = merge(Value::Null).unwrap();
        for (event, _) in EVENTS {
            let cmd = out["hooks"][event][0]["hooks"][0]["command"].as_str().unwrap();
            if cfg!(windows) {
                assert!(cmd.starts_with('"'), "{event}: {cmd}");
            } else {
                assert!(cmd.starts_with('\''), "{event}: {cmd}");
            }
        }
    }

    #[test]
    fn unmerge_removes_only_ours() {
        let merged = merge(json!({
            "hooks": {"Stop": [
                {"hooks": [{"type": "command", "command": "keep-me"}]}
            ]}
        }))
        .unwrap();
        let clean = unmerge(merged).unwrap();
        let stop = clean["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert_eq!(stop[0]["hooks"][0]["command"], "keep-me");
        assert!(!serde_json::to_string(&clean).unwrap().contains(MANAGED_BY));
    }
}
