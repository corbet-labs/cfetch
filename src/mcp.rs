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

use crate::{answer, code, config::Config, index, paths, serve};

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
            "Search the operator's knowledge brain (privilege rings 0-4, BM25-ranked). Returns ring-prefixed citations (r<ring>-<hash>), file:line locations and snippets. Lower ring = higher trust; a ring-0/1 hit overrides contradicting outer-ring content. The answer is capped by a token budget, not only by limit: whatever the cap drops is named at the end, never omitted silently.",
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
            "Expand a citation id from cfetch_recall to the full memory block. Long blocks are clipped to a token budget and the answer then names the file:line range holding the rest — read that range directly instead of expanding again.",
            object_schema(json!({
                "type": "object",
                "properties": {"cite": {"type": "string"}},
                "required": ["cite"]
            })),
        )
        .with_annotations(read_only()),
        Tool::new(
            "cfetch_find",
            "Locate a symbol or file in the indexed code roots, with exact line ranges — read one function instead of a whole file. Case- and separator-insensitive. Each hit carries the estimated cost of reading it; the answer itself is capped by a token budget and says what the cap dropped.",
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

/// Wire and local hits carry the same fields under different types (the
/// serving protocol owns one, the index the other), so each answer kind gets
/// one adapter per shape and exactly one renderer.
fn recall_entry(h: &serve::WireHit) -> String {
    answer::hit_entry(&h.cite, &h.path, h.ring, h.start_line, h.end_line, &h.snippet, &h.mirrors)
}

fn local_recall_entry(h: &index::Hit) -> String {
    answer::hit_entry(&h.cite, &h.path, h.ring, h.start_line, h.end_line, &h.snippet, &h.mirrors)
}

fn expand_entry(b: &serve::WireBlock) -> answer::BlockIn {
    answer::BlockIn {
        cite: b.cite.clone(),
        path: b.path.clone(),
        ring: b.ring,
        start_line: b.start_line,
        end_line: b.end_line,
        text: b.text.clone(),
    }
}

fn local_expand_entry(b: &index::Block) -> answer::BlockIn {
    answer::BlockIn {
        cite: b.cite.clone(),
        path: b.path.clone(),
        ring: b.ring,
        start_line: b.start_line,
        end_line: b.end_line,
        text: b.text.clone(),
    }
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
                answer::listing(
                    hits.iter().map(recall_entry).collect(),
                    answer::RECALL_BUDGET_TOKENS,
                    answer::MCP_RECOVERY,
                )
            }
        }
        "cfetch_expand" => {
            let blocks = resp.blocks.clone().unwrap_or_default();
            if blocks.is_empty() {
                "no block with that citation".to_string()
            } else {
                answer::blocks(
                    &blocks.iter().map(expand_entry).collect::<Vec<_>>(),
                    answer::RECALL_BUDGET_TOKENS,
                )
            }
        }
        _ => {
            let hits = resp.code_hits.clone().unwrap_or_default();
            if hits.is_empty() {
                "no hits".to_string()
            } else {
                answer::listing(
                    hits.iter()
                        .map(|h| {
                            answer::find_entry(
                                &h.path,
                                h.name.as_deref(),
                                h.kind.as_deref(),
                                h.start_line,
                                h.end_line,
                                h.token_estimate,
                            )
                        })
                        .collect(),
                    answer::FIND_BUDGET_TOKENS,
                    answer::MCP_RECOVERY,
                )
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
            Ok(answer::listing(
                hits.iter().map(local_recall_entry).collect(),
                answer::RECALL_BUDGET_TOKENS,
                answer::MCP_RECOVERY,
            ))
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
            Ok(answer::blocks(
                &blocks.iter().map(local_expand_entry).collect::<Vec<_>>(),
                answer::RECALL_BUDGET_TOKENS,
            ))
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
            Ok(answer::listing(
                hits.iter()
                    .map(|h| {
                        answer::find_entry(
                            &h.path,
                            h.name.as_deref(),
                            h.kind.as_deref(),
                            h.start_line,
                            h.end_line,
                            h.token_estimate,
                        )
                    })
                    .collect(),
                answer::FIND_BUDGET_TOKENS,
                answer::MCP_RECOVERY,
            ))
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
    fn every_tool_tells_the_model_its_answer_is_capped() {
        // A cap the caller cannot see reads as a broken index: the model
        // re-asks the same question instead of following the file:line
        // pointer the truncated answer already handed it.
        for tool in tool_defs() {
            let description = tool.description.as_deref().unwrap_or_default();
            assert!(
                description.contains("token budget"),
                "{} hides its answer budget: {description}",
                tool.name
            );
        }
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
