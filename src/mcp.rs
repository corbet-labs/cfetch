//! MCP server (stdio JSON-RPC 2.0): the one-protocol answer to "support all
//! common agents" — Claude Desktop, Codex CLI, Gemini CLI, Cursor and any MCP
//! client get recall/find/expand without per-agent hook dialects.
//!
//! Hand-rolled on purpose: the protocol subset we serve (initialize,
//! tools/list, tools/call, ping) is ~200 lines; an SDK dependency would be
//! larger than the feature. Tools are READ-ONLY.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

use crate::{code, config::Config, index, paths};

const PROTOCOL_FALLBACK: &str = "2025-06-18";

fn tool_defs() -> Value {
    json!([
        {
            "name": "cfetch_recall",
            "description": "Search the operator's knowledge brain (privilege rings 0-4, BM25-ranked). Returns ring-prefixed citations (r<ring>-<hash>), file:line locations and snippets. Lower ring = higher trust; a ring-0/1 hit overrides contradicting outer-ring content.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "search terms (word-prefix matched)"},
                    "limit": {"type": "integer", "default": 8}
                },
                "required": ["query"]
            }
        },
        {
            "name": "cfetch_expand",
            "description": "Expand a citation id from cfetch_recall to the full memory block.",
            "inputSchema": {
                "type": "object",
                "properties": {"cite": {"type": "string"}},
                "required": ["cite"]
            }
        },
        {
            "name": "cfetch_find",
            "description": "Locate a symbol or file in the indexed code roots, with exact line ranges — read one function instead of a whole file. Case- and separator-insensitive.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "default": 10}
                },
                "required": ["query"]
            }
        }
    ])
}

fn text_result(text: String) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": false})
}

fn run_tool(name: &str, args: &Value) -> anyhow::Result<String> {
    let cfg = Config::load()?;
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(0) as usize;
    match name {
        "cfetch_recall" => {
            let query = args.get("query").and_then(Value::as_str).unwrap_or("");
            let native = paths::native_projects_root();
            let conn = index::ensure_fresh(&paths::state_dir(), &cfg.brain_root, Some(&native))?;
            let hits = index::recall(&conn, query, if limit == 0 { 8 } else { limit })?;
            if hits.is_empty() {
                return Ok(format!("no hits for \"{query}\""));
            }
            Ok(hits
                .iter()
                .map(|h| {
                    format!(
                        "{} {}:{}-{} (ring {})\n    {}",
                        h.cite, h.path, h.start_line, h.end_line, h.ring, h.snippet
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "cfetch_expand" => {
            let cite = args.get("cite").and_then(Value::as_str).unwrap_or("");
            let native = paths::native_projects_root();
            let conn = index::ensure_fresh(&paths::state_dir(), &cfg.brain_root, Some(&native))?;
            let blocks = index::expand(&conn, cite)?;
            if blocks.is_empty() {
                return Ok(format!("no block with citation {cite}"));
            }
            Ok(blocks
                .iter()
                .map(|b| format!("{} {}:{}-{} (ring {})\n{}", b.cite, b.path, b.start_line, b.end_line, b.ring, b.text))
                .collect::<Vec<_>>()
                .join("\n\n"))
        }
        "cfetch_find" => {
            let query = args.get("query").and_then(Value::as_str).unwrap_or("");
            // Snapshot only — an implicit code scan (minutes on NFS) must
            // never ride on an interactive tool call.
            let conn = index::open(&paths::state_dir())?;
            let hits = code::find(&conn, query, if limit == 0 { 10 } else { limit })?;
            if hits.is_empty() {
                return Ok(format!("no hits for \"{query}\""));
            }
            Ok(hits
                .iter()
                .map(|h| match (&h.name, &h.kind) {
                    (Some(n), Some(k)) => format!(
                        "{}:{}-{}  {} {}  (~{} tok)",
                        h.path, h.start_line, h.end_line, k, n, h.token_estimate
                    ),
                    _ => format!("{}  (file match)", h.path),
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        other => anyhow::bail!("unknown tool: {other}"),
    }
}

/// Handles one JSON-RPC message. Returns None for notifications (no id) —
/// they get no response by contract.
pub fn handle(msg: &Value) -> Option<Value> {
    let id = msg.get("id")?.clone();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let respond = |v: Value| Some(json!({"jsonrpc": "2.0", "id": id, "result": v}));
    match method {
        "initialize" => {
            let requested = msg
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_FALLBACK);
            respond(json!({
                "protocolVersion": requested,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "cfetch", "version": env!("CARGO_PKG_VERSION")}
            }))
        }
        "ping" => respond(json!({})),
        "tools/list" => respond(json!({"tools": tool_defs()})),
        "tools/call" => {
            let name = msg.pointer("/params/name").and_then(Value::as_str).unwrap_or("");
            let empty = json!({});
            let args = msg.pointer("/params/arguments").unwrap_or(&empty);
            match run_tool(name, args) {
                Ok(text) => respond(text_result(text)),
                Err(e) => respond(json!({
                    "content": [{"type": "text", "text": format!("error: {e}")}],
                    "isError": true
                })),
            }
        }
        _ => Some(json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": -32601, "message": format!("method not found: {method}")}
        })),
    }
}

/// Stdio serve loop: one JSON-RPC message per line in, one per line out.
pub fn serve() -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        if let Some(resp) = handle(&msg) {
            writeln!(stdout, "{resp}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_echoes_protocol_and_lists_tools() {
        let resp = handle(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-03-26"}
        }))
        .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(resp["result"]["serverInfo"]["name"], "cfetch");

        let tools = handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).unwrap();
        let names: Vec<&str> = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["cfetch_recall", "cfetch_expand", "cfetch_find"]);
    }

    #[test]
    fn notifications_get_no_response() {
        assert!(handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none());
    }

    #[test]
    fn unknown_method_is_a_jsonrpc_error() {
        let resp = handle(&json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list"})).unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[test]
    fn unknown_tool_reports_is_error_not_crash() {
        let resp = handle(&json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "cfetch_delete_everything", "arguments": {}}
        }))
        .unwrap();
        assert_eq!(resp["result"]["isError"], true);
    }
}
