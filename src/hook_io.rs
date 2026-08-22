//! The shared Claude Code / Codex hook I/O contract, encoded in types.
//!
//! Hard-won rules (see DESIGN.md, "hooks contract"):
//! - stdin is ONE JSON object; field names have drifted across harness versions,
//!   so every event field is optional and unknown fields are ignored.
//! - stdout must be at most ONE JSON object; two concatenated objects are
//!   silently dropped by the harness. `Emit` enforces single emission by move.
//! - stderr from an exit-0 hook never reaches the model.
//! - `permissionDecision: "allow"` would bypass the user's permission system;
//!   the type simply has no allow variant.
//! - Internal failures exit 0. An accidental exit 2 from a Stop hook traps the
//!   session in a loop; nonzero from SessionStart spams error notices.

use std::io::Read;

use serde::{Deserialize, Serialize};

/// Every event, one lenient shape. The harness has renamed fields across
/// versions (SessionStart `source`->`reason`, Stop `stop_hook_active`->
/// `stop_reason`), so both spellings are kept where relevant.
#[derive(Debug, Default, Clone, Deserialize)]
#[allow(dead_code)] // this struct IS the wire contract; later milestones read the rest
pub struct HookEvent {
    pub session_id: Option<String>,
    pub transcript_path: Option<String>,
    pub cwd: Option<String>,
    pub hook_event_name: Option<String>,
    pub permission_mode: Option<String>,
    // SessionStart: current name, then the historical one.
    pub reason: Option<String>,
    pub source: Option<String>,
    // PreToolUse / PostToolUse
    pub tool_name: Option<String>,
    pub tool_input: Option<serde_json::Value>,
    pub tool_response: Option<serde_json::Value>,
    pub tool_use_id: Option<String>,
    /// Some harness versions surface tool failure as its own field.
    pub tool_error: Option<serde_json::Value>,
    // Stop
    pub stop_hook_active: Option<bool>,
    pub stop_reason: Option<String>,
    // PreCompact
    pub trigger: Option<String>,
    // Subagent invocations
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
}

impl HookEvent {
    /// Reads the event from stdin. Any failure yields the empty event — a hook
    /// must degrade, never crash the harness interaction.
    pub fn from_stdin() -> HookEvent {
        let mut buf = String::new();
        if std::io::stdin().read_to_string(&mut buf).is_err() {
            return HookEvent::default();
        }
        serde_json::from_str(&buf).unwrap_or_default()
    }

    /// SessionStart reason with the historical field as fallback.
    pub fn start_reason(&self) -> &str {
        self.reason.as_deref().or(self.source.as_deref()).unwrap_or("startup")
    }

    pub fn session(&self) -> &str {
        self.session_id.as_deref().unwrap_or("unknown-session")
    }

    pub fn is_subagent(&self) -> bool {
        self.agent_id.is_some() || self.agent_type.is_some()
    }
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookSpecificOutput {
    pub hook_event_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct HookOutput {
    #[serde(skip_serializing_if = "Option::is_none", rename = "systemMessage")]
    pub system_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "suppressOutput")]
    pub suppress_output: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "hookSpecificOutput")]
    pub hook_specific_output: Option<HookSpecificOutput>,
}

/// Single-emission stdout writer. `finish` consumes the builder; there is no
/// way to write two objects, and an empty builder writes nothing at all.
pub struct Emit {
    event_name: &'static str,
    context: Vec<String>,
    system_message: Option<String>,
}

impl Emit {
    pub fn new(event_name: &'static str) -> Emit {
        Emit { event_name, context: Vec::new(), system_message: None }
    }

    pub fn add_context(&mut self, s: impl Into<String>) {
        let s = s.into();
        if !s.trim().is_empty() {
            self.context.push(s);
        }
    }

    #[allow(dead_code)] // used from Milestone 5 governance paths
    pub fn system_message(&mut self, s: impl Into<String>) {
        self.system_message = Some(s.into());
    }

    /// Returns the number of context characters actually emitted, for booking.
    pub fn finish(self) -> usize {
        if self.context.is_empty() && self.system_message.is_none() {
            return 0;
        }
        let joined = self.context.join("\n\n");
        let emitted = joined.len();
        let out = HookOutput {
            system_message: self.system_message,
            suppress_output: None,
            hook_specific_output: if joined.is_empty() {
                None
            } else {
                Some(HookSpecificOutput {
                    hook_event_name: self.event_name.to_string(),
                    additional_context: Some(joined),
                })
            },
        };
        // A serialization failure means we print nothing — never half an object.
        if let Ok(s) = serde_json::to_string(&out) {
            println!("{s}");
        }
        emitted
    }
}

/// Estimated tokens for booked injections. A labeled heuristic (chars/3.5) —
/// measured transcript usage supersedes it in Milestone 5.
pub fn estimate_tokens(chars: usize) -> u64 {
    ((chars as f64) / 3.5).ceil() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unknown_fields_and_renames() {
        let e: HookEvent = serde_json::from_str(
            r#"{"session_id":"s1","reason":"resume","brand_new_field":42,"tool_input":{"command":"ls"}}"#,
        )
        .unwrap();
        assert_eq!(e.session(), "s1");
        assert_eq!(e.start_reason(), "resume");
        let old: HookEvent = serde_json::from_str(r#"{"source":"compact"}"#).unwrap();
        assert_eq!(old.start_reason(), "compact");
    }

    #[test]
    fn garbage_stdin_degrades_to_default() {
        let e: HookEvent = serde_json::from_str("not json").unwrap_or_default();
        assert_eq!(e.session(), "unknown-session");
        assert_eq!(e.start_reason(), "startup");
    }

    #[test]
    fn empty_emit_writes_nothing_and_books_zero() {
        let emitted = Emit::new("SessionStart").finish();
        assert_eq!(emitted, 0);
    }

    #[test]
    fn output_shape_is_camel_case() {
        let out = HookOutput {
            system_message: None,
            suppress_output: None,
            hook_specific_output: Some(HookSpecificOutput {
                hook_event_name: "SessionStart".into(),
                additional_context: Some("x".into()),
            }),
        };
        let s = serde_json::to_string(&out).unwrap();
        assert!(s.contains("hookSpecificOutput"));
        assert!(s.contains("hookEventName"));
        assert!(s.contains("additionalContext"));
    }

    #[test]
    fn token_estimate_is_ceiled() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(7), 2);
    }
}
