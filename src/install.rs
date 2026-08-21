//! Registers cfetch's hooks in Claude Code's settings.json using the
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
    ("PreToolUse", "pre-tool"),
    ("PostToolUse", "post-tool"),
    ("Stop", "stop"),
    ("PreCompact", "precompact"),
];

pub fn default_settings_path() -> PathBuf {
    paths::home().join(".claude/settings.json")
}

/// The command embeds the absolute binary path: hooks run outside a login
/// shell, so PATH is not a contract. POSIX-single-quoted against spaces.
fn hook_command(subcommand: &str) -> String {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "cfetch".to_string());
    format!("'{}' hook {subcommand}", exe.replace('\'', r"'\''"))
}

fn managed_entry(subcommand: &str) -> Value {
    json!({
        "hooks": [{
            "type": "command",
            "command": hook_command(subcommand),
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

/// Pure merge so it is testable: returns the new settings document.
pub fn merge(settings: Value) -> anyhow::Result<Value> {
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
        list.push(managed_entry(subcommand));
    }
    Ok(Value::Object(root))
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
