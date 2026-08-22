//! MCP server: the one-protocol answer to supporting every MCP-capable agent.
//!
//! cfetch owns the read-only tool behavior. The official Rust MCP SDK owns
//! JSON-RPC framing, lifecycle, version negotiation, capability discovery,
//! schema validation, and stdio concurrency.

use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
        ToolAnnotations,
    },
    service::{RequestContext, RoleServer},
};
use serde_json::{Value, json};

use crate::{code, config::Config, index, paths, serve};

/// The tools we serve; a tools/call naming anything else is the caller's
/// protocol error (-32602), not a tool-execution failure.
const TOOL_NAMES: &[&str] = &["cfetch_recall", "cfetch_expand", "cfetch_find"];

fn object_schema(value: Value) -> JsonObject {
    value
        .as_object()
        .cloned()
        .expect("tool schema is an object")
}

fn tool_defs() -> Vec<Tool> {
    let read_only = || {
        ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(true)
            .open_world(false)
    };
    vec![
        Tool::new(
            "cfetch_recall",
            "Search the operator's knowledge brain (privilege rings 0-4, BM25-ranked). Returns ring-prefixed citations (r<ring>-<hash>), file:line locations and snippets. Lower ring = higher trust; a ring-0/1 hit overrides contradicting outer-ring content.",
            object_schema(json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "search terms (word-prefix matched)"},
                    "limit": {"type": "integer", "default": 8}
                },
                "required": ["query"]
            })),
        )
        .with_annotations(read_only()),
        Tool::new(
            "cfetch_expand",
            "Expand a citation id from cfetch_recall to the full memory block.",
            object_schema(json!({
                "type": "object",
                "properties": {"cite": {"type": "string"}},
                "required": ["cite"]
            })),
        )
        .with_annotations(read_only()),
        Tool::new(
            "cfetch_find",
            "Locate a symbol or file in the indexed code roots, with exact line ranges — read one function instead of a whole file. Case- and separator-insensitive.",
            object_schema(json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "default": 10}
                },
                "required": ["query"]
            })),
        )
        .with_annotations(read_only()),
    ]
}

/// Coherence footer appended to every remotely-served MCP answer.
fn served_footer(resp: &crate::daemon::Response) -> String {
    let origin = resp.origin.clone().unwrap_or_default();
    let generation = resp.generation.unwrap_or(0);
    if resp.fresh == Some(false) {
        format!(
            "\n[served by {origin}, generation {generation} — STALE: {}]",
            resp.stale_note
                .clone()
                .unwrap_or_else(|| "barrier expired".to_string())
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
                        format!(
                            "{} {}:{}-{} (ring {})\n{}",
                            b.cite, b.path, b.start_line, b.end_line, b.ring, b.text
                        )
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
            let conn = index::ensure_fresh(
                &paths::state_dir(),
                &cfg.brain_root,
                Some(&native),
                &cfg.rings(),
            )?;
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
            let conn = index::ensure_fresh(
                &paths::state_dir(),
                &cfg.brain_root,
                Some(&native),
                &cfg.rings(),
            )?;
            let blocks = index::expand(&conn, cite)?;
            if blocks.is_empty() {
                return Ok(format!("no block with citation {cite}"));
            }
            Ok(blocks
                .iter()
                .map(|b| {
                    format!(
                        "{} {}:{}-{} (ring {})\n{}",
                        b.cite, b.path, b.start_line, b.end_line, b.ring, b.text
                    )
                })
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

fn call_tool(name: &str, args: &Value) -> Result<CallToolResult, McpError> {
    if !TOOL_NAMES.contains(&name) {
        return Err(McpError::invalid_params(format!("unknown tool: {name}"), None));
    }
    Ok(match run_tool(name, args) {
        Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
        Err(error) => CallToolResult::error(vec![ContentBlock::text(format!("error: {error}"))]),
    })
}

struct CfetchMcp;

impl ServerHandler for CfetchMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("cfetch", env!("CARGO_PKG_VERSION")))
            // One doctrine, two renderers: this same source function feeds
            // the AGENTS.md/GEMINI.md marker block.
            .with_instructions(crate::markers::doctrine(crate::markers::Surface::Mcp))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(tool_defs()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tool_defs().into_iter().find(|tool| tool.name == name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let name = request.name.into_owned();
        if !TOOL_NAMES.contains(&name.as_str()) {
            return Err(McpError::invalid_params(format!("unknown tool: {name}"), None));
        }
        let args = Value::Object(request.arguments.unwrap_or_default());
        let result = tokio::task::spawn_blocking(move || call_tool(&name, &args))
            .await
            .map_err(|error| McpError::internal_error(format!("tool worker failed: {error}"), None))??;
        Ok(result.into())
    }
}

/// Serve over the SDK's stdio transport until the client closes the session.
pub fn serve() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("cfetch-mcp")
        .build()?;
    runtime.block_on(async {
        let service = CfetchMcp.serve(rmcp::transport::stdio()).await?;
        service.waiting().await?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_info_and_tools_are_one_coherent_contract() {
        let info = CfetchMcp.get_info();
        assert_eq!(info.server_info.name, "cfetch");
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
        assert!(info.capabilities.tools.is_some());
        let tools = tool_defs();
        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
        assert_eq!(
            names, TOOL_NAMES,
            "tools/list and the -32602 gate must agree"
        );
        assert!(tools.iter().all(|tool| {
            tool.annotations
                .as_ref()
                .and_then(|annotations| annotations.read_only_hint)
                == Some(true)
        }));
        let instructions = info.instructions.as_deref().unwrap();
        // Same source function as the AGENTS.md/GEMINI.md marker block.
        assert_eq!(
            instructions,
            crate::markers::doctrine(crate::markers::Surface::Mcp)
        );
        assert!(
            instructions.contains("cfetch_recall"),
            "MCP surface names the MCP tools"
        );
        assert!(instructions.contains("Before searching files or reading code wholesale"));
    }

    #[test]
    fn unknown_tool_is_a_jsonrpc_invalid_params_error() {
        let error = call_tool("cfetch_delete_everything", &json!({})).unwrap_err();
        assert_eq!(
            error.code.0, -32602,
            "protocol error, not an isError result"
        );
    }
}
