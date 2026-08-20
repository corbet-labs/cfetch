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

fn managed_entry(subcommand: &str) -> Value {
    json!({
        "hooks": [{
            "type": "command",
            "command": format!("cfetch hook {subcommand}"),
            "timeout": 10,
            "_managedBy": MANAGED_BY,
        }]
    })
}

fn is_managed(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| {
            hooks
                .iter()
                .any(|h| h.get("_managedBy").and_then(Value::as_str) == Some(MANAGED_BY))
        })
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
        list.retain(|entry| !is_managed(entry));
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
                arr.retain(|entry| !is_managed(entry));
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
        for (event, _) in EVENTS {
            let arr = clean["hooks"][event].as_array().unwrap();
            assert!(arr.iter().all(|e| !is_managed(e)));
        }
    }
}
