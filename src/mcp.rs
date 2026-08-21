//! MCP server (stdio JSON-RPC 2.0): the one-protocol answer to "support all
//! common agents" — Claude Desktop, Codex CLI, Gemini CLI, Cursor and any MCP
//! client get recall/find/expand without per-agent hook dialects.
//!
//! Hand-rolled on purpose: the protocol subset we serve (initialize,
//! tools/list, tools/call, ping) is ~200 lines; an SDK dependency would be
//! larger than the feature. Tools are READ-ONLY.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

use crate::{code, config::Config, index, paths, serve};

/// Protocol revisions this server implements, newest first. Negotiation per
/// the MCP spec: a version we support is echoed back; anything else (or an
/// absent field) is answered with OUR newest supported version — the client
/// then decides whether it can proceed. Never a blind echo.
const SUPPORTED_PROTOCOLS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// The tools we serve; a tools/call naming anything else is the caller's
/// protocol error (-32602), not a tool-execution failure.
const TOOL_NAMES: &[&str] = &["cfetch_recall", "cfetch_expand", "cfetch_find"];

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

/// Coherence footer appended to every remotely-served MCP answer.
fn served_footer(resp: &crate::daemon::Response) -> String {
    let origin = resp.origin.clone().unwrap_or_default();
    let generation = resp.generation.unwrap_or(0);
    if resp.fresh == Some(false) {
        format!(
            "\n[served by {origin}, generation {generation} — STALE: {}]",
            resp.stale_note.clone().unwrap_or_else(|| "barrier expired".to_string())
        )
    } else {
        format!("\n[served by {origin}, generation {generation}, fresh]")
    }
}

/// none-tier routing: every tool answered by the remote serving host; no
/// local index is opened. Unreachable = explicit error naming the host.
fn run_tool_remote(
    cs: &crate::config::ClientServingConfig,
    name: &str,
    args: &Value,
    limit: usize,
) -> anyhow::Result<String> {
    let body = match name {
        "cfetch_recall" => json!({
            "op": "recall",
            "query": args.get("query").and_then(Value::as_str).unwrap_or(""),
            "limit": if limit == 0 { 8 } else { limit },
        }),
        "cfetch_expand" => json!({
            "op": "expand",
            "cite": args.get("cite").and_then(Value::as_str).unwrap_or(""),
        }),
        "cfetch_find" => json!({
            "op": "find",
            "query": args.get("query").and_then(Value::as_str).unwrap_or(""),
            "limit": if limit == 0 { 10 } else { limit },
        }),
        other => anyhow::bail!("unknown tool: {other}"),
    };
    let resp = serve::client_call(cs, body, serve::QUERY_TIMEOUT)?;
    let text = match name {
        "cfetch_recall" => {
            let hits = resp.hits.clone().unwrap_or_default();
            if hits.is_empty() {
                "no hits".to_string()
            } else {
                hits.iter()
                    .map(|h| {
                        let mut line = format!(
                            "{} {}:{}-{} (ring {})\n    {}",
                            h.cite, h.path, h.start_line, h.end_line, h.ring, h.snippet
                        );
                        if !h.mirrors.is_empty() {
                            line.push_str(&format!("\n    (also at: {})", h.mirrors.join(", ")));
                        }
                        line
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        "cfetch_expand" => {
            let blocks = resp.blocks.clone().unwrap_or_default();
            if blocks.is_empty() {
                "no block with that citation".to_string()
            } else {
                blocks
                    .iter()
                    .map(|b| {
                        format!("{} {}:{}-{} (ring {})\n{}", b.cite, b.path, b.start_line, b.end_line, b.ring, b.text)
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n")
            }
        }
        _ => {
            let hits = resp.code_hits.clone().unwrap_or_default();
            if hits.is_empty() {
                "no hits".to_string()
            } else {
                hits.iter()
                    .map(|h| match (&h.name, &h.kind) {
                        (Some(n), Some(k)) => format!(
                            "{}:{}-{}  {} {}  (~{} tok)",
                            h.path, h.start_line, h.end_line, k, n, h.token_estimate
                        ),
                        _ => format!("{}  (file match)", h.path),
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
    };
    Ok(format!("{text}{}", served_footer(&resp)))
}

fn run_tool(name: &str, args: &Value) -> anyhow::Result<String> {
    let cfg = Config::load()?;
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(0) as usize;
    if let Some(cs) = &cfg.client.serving {
        return run_tool_remote(cs, name, args, limit);
    }
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
                    let mut line = format!(
                        "{} {}:{}-{} (ring {})\n    {}",
                        h.cite, h.path, h.start_line, h.end_line, h.ring, h.snippet
                    );
                    if !h.mirrors.is_empty() {
                        line.push_str(&format!("\n    (also at: {})", h.mirrors.join(", ")));
                    }
                    line
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
            let requested = msg.pointer("/params/protocolVersion").and_then(Value::as_str);
            let negotiated = match requested {
                Some(v) if SUPPORTED_PROTOCOLS.contains(&v) => v,
                _ => SUPPORTED_PROTOCOLS[0],
            };
            respond(json!({
                "protocolVersion": negotiated,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "cfetch", "version": env!("CARGO_PKG_VERSION")},
                // One doctrine, two renderers: the same source function feeds
                // the AGENTS.md/GEMINI.md marker block (markers::protocol_block).
                "instructions": crate::markers::doctrine(crate::markers::Surface::Mcp),
            }))
        }
        "ping" => respond(json!({})),
        "tools/list" => respond(json!({"tools": tool_defs()})),
        "tools/call" => {
            let name = msg.pointer("/params/name").and_then(Value::as_str).unwrap_or("");
            if !TOOL_NAMES.contains(&name) {
                // Unknown tool = invalid params per the MCP spec, a JSON-RPC
                // -32602 error. `isError: true` results are reserved for
                // failures of a REAL tool's execution (those the model can
                // see and react to).
                return Some(json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32602, "message": format!("unknown tool: {name}")}
                }));
            }
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

/// Handles one wire message, which JSON-RPC 2.0 allows to be a batch array:
/// a batch gets a batch response (notifications contribute nothing, and an
/// all-notification batch gets no response at all); the empty batch is the
/// spec's invalid-request error.
pub fn handle_message(msg: &Value) -> Option<Value> {
    match msg.as_array() {
        Some(items) => {
            if items.is_empty() {
                return Some(json!({
                    "jsonrpc": "2.0", "id": Value::Null,
                    "error": {"code": -32600, "message": "invalid request: empty batch"}
                }));
            }
            let responses: Vec<Value> = items.iter().filter_map(handle).collect();
            (!responses.is_empty()).then_some(Value::Array(responses))
        }
        None => handle(msg),
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
        if let Some(resp) = handle_message(&msg) {
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
    fn initialize_negotiates_protocol_and_lists_tools() {
        // A supported (older) revision is accepted and echoed.
        let resp = handle(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-03-26"}
        }))
        .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(resp["result"]["serverInfo"]["name"], "cfetch");

        // An unknown revision gets OUR newest supported, never a blind echo.
        let resp = handle(&json!({
            "jsonrpc": "2.0", "id": 2, "method": "initialize",
            "params": {"protocolVersion": "1999-01-01"}
        }))
        .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], SUPPORTED_PROTOCOLS[0]);

        // Absent field: same answer as unknown.
        let resp = handle(&json!({"jsonrpc": "2.0", "id": 3, "method": "initialize"})).unwrap();
        assert_eq!(resp["result"]["protocolVersion"], SUPPORTED_PROTOCOLS[0]);

        let tools = handle(&json!({"jsonrpc": "2.0", "id": 4, "method": "tools/list"})).unwrap();
        let names: Vec<&str> = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, TOOL_NAMES, "tools/list and the -32602 gate must agree");
    }

    #[test]
    fn initialize_carries_recall_first_instructions() {
        let resp = handle(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18"}
        }))
        .unwrap();
        let instructions = resp["result"]["instructions"].as_str().unwrap();
        // Same source function as the AGENTS.md/GEMINI.md marker block.
        assert_eq!(instructions, crate::markers::doctrine(crate::markers::Surface::Mcp));
        assert!(instructions.contains("cfetch_recall"), "MCP surface names the MCP tools");
        assert!(instructions.contains("Before searching files or reading code wholesale"));
    }

    #[test]
    fn batch_requests_get_a_batch_response() {
        let batch = json!([
            {"jsonrpc": "2.0", "id": 1, "method": "ping"},
            {"jsonrpc": "2.0", "method": "notifications/initialized"},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/list"}
        ]);
        let resp = handle_message(&batch).unwrap();
        let arr = resp.as_array().expect("a batch request gets a batch response");
        assert_eq!(arr.len(), 2, "the notification contributes no response");
        assert_eq!(arr[0]["id"], 1);
        assert_eq!(arr[1]["id"], 2);
        assert!(arr[1]["result"]["tools"].is_array());

        // All-notification batch: no response at all.
        let silent = json!([{"jsonrpc": "2.0", "method": "notifications/initialized"}]);
        assert!(handle_message(&silent).is_none());

        // Empty batch: the spec's single invalid-request error.
        let err = handle_message(&json!([])).unwrap();
        assert_eq!(err["error"]["code"], -32600);

        // A single (non-array) message passes through unchanged.
        let single = handle_message(&json!({"jsonrpc": "2.0", "id": 9, "method": "ping"})).unwrap();
        assert_eq!(single["id"], 9);
        assert!(!single.is_array());
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
    fn unknown_tool_is_a_jsonrpc_invalid_params_error() {
        let resp = handle(&json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "cfetch_delete_everything", "arguments": {}}
        }))
        .unwrap();
        assert_eq!(resp["error"]["code"], -32602, "protocol error, not an isError result");
        assert!(resp.get("result").is_none());
        assert_eq!(resp["id"], 4);
    }
}
