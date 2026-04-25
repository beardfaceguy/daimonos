use async_trait::async_trait;
use rust_mcp_sdk::mcp_server::{
    server_runtime, McpServerOptions, ServerHandler, ToMcpServerHandler,
};
use rust_mcp_sdk::schema::{
    CallToolError, CallToolRequestParams, CallToolResult, Implementation, InitializeResult,
    ListToolsResult, PaginatedRequestParams, ProtocolVersion, RpcError, ServerCapabilities,
    ServerCapabilitiesTools, TextContent, Tool,
};
use rust_mcp_sdk::{McpServer, StdioTransport, TransportOptions};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::index::WorkspaceIndex;
use crate::ops;
use crate::pipeline_cache::PipelineCache;
use crate::protocol::{self, Op, Request, Response};
use crate::session::Session;
use crate::tool_runner::ToolRegistry;

pub struct DaimonosHandler {
    session: Arc<Mutex<Session>>,
}

impl DaimonosHandler {
    pub fn new(session: Session) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
        }
    }
}

// --- Argument extraction helpers ---

fn get_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)?.as_str().map(String::from)
}

fn get_i64(args: &Value, key: &str) -> Option<i64> {
    args.get(key)?.as_i64()
}

fn get_str_array(args: &Value, key: &str) -> Option<Vec<String>> {
    args.get(key)?
        .as_array()?
        .iter()
        .map(|v| v.as_str().map(String::from))
        .collect()
}

fn get_str_map(args: &Value, key: &str) -> Option<std::collections::HashMap<String, String>> {
    let obj = args.get(key)?.as_object()?;
    let mut map = std::collections::HashMap::new();
    for (k, v) in obj {
        if let Some(s) = v.as_str() {
            map.insert(k.clone(), s.to_string());
        }
    }
    Some(map)
}

fn ok_text(text: String) -> std::result::Result<CallToolResult, CallToolError> {
    Ok(CallToolResult::text_content(vec![TextContent::new(
        text, None, None,
    )]))
}

fn err_text(msg: String) -> std::result::Result<CallToolResult, CallToolError> {
    let mut result = CallToolResult::text_content(vec![TextContent::new(msg, None, None)]);
    result.is_error = Some(true);
    Ok(result)
}

fn response_to_result(resp: Response) -> std::result::Result<CallToolResult, CallToolError> {
    if resp.ok {
        let text = match resp.d {
            Some(data) => serde_json::to_string(&data).unwrap_or_default(),
            None => "{}".to_string(),
        };
        ok_text(text)
    } else {
        let msg = resp.m.unwrap_or_else(|| "unknown error".into());
        let code = resp.e.unwrap_or(0);
        err_text(format!("error {code}: {msg}"))
    }
}

async fn dispatch_tool(
    session: &mut Session,
    name: &str,
    args: &Value,
) -> std::result::Result<CallToolResult, CallToolError> {
    // Auto-activate extended tools on first use
    if !session.exposed_tools.contains(name) {
        session.activate_tool(name);
    }

    match name {
        "list_all_tools" => {
            session.activate_all_tools();
            let all = tool_definitions();
            let summary: Vec<Value> = all
                .iter()
                .map(|t| json!({"name": t.name, "description": t.description}))
                .collect();
            ok_text(serde_json::to_string(&summary).unwrap_or_default())
        }

        "read_file" => {
            let resp = ops::dispatch(
                session,
                Request::Single(Op {
                    c: protocol::op::READ,
                    p: get_str(args, "path"),
                    n: get_i64(args, "offset"),
                    n2: get_i64(args, "limit"),
                    ..Default::default()
                }),
            )
            .await;
            response_to_result(resp)
        }

        "write_file" => {
            let resp = ops::dispatch(
                session,
                Request::Single(Op {
                    c: protocol::op::WRITE,
                    p: get_str(args, "path"),
                    s: get_str(args, "content"),
                    ..Default::default()
                }),
            )
            .await;
            response_to_result(resp)
        }

        "edit_file" => {
            let resp = ops::dispatch(
                session,
                Request::Single(Op {
                    c: protocol::op::PATCH,
                    p: get_str(args, "path"),
                    a: get_str_array(args, "edits"),
                    ..Default::default()
                }),
            )
            .await;
            response_to_result(resp)
        }

        "search" => {
            let mode = get_str(args, "mode").unwrap_or_else(|| "content".into());

            let resp = if mode == "files" {
                ops::dispatch(
                    session,
                    Request::Single(Op {
                        c: protocol::op::FIND,
                        p: get_str(args, "pattern"),
                        n: get_i64(args, "max_results"),
                        ..Default::default()
                    }),
                )
                .await
            } else {
                ops::dispatch(
                    session,
                    Request::Single(Op {
                        c: protocol::op::GREP,
                        p: get_str(args, "pattern"),
                        q: get_str(args, "path"),
                        g: get_str(args, "glob"),
                        n: get_i64(args, "max_results"),
                        ..Default::default()
                    }),
                )
                .await
            };
            response_to_result(resp)
        }

        "workspace_info" => {
            let session_resp = ops::dispatch(
                session,
                Request::Single(Op {
                    c: protocol::op::SESSION,
                    ..Default::default()
                }),
            )
            .await;

            let ls_resp = ops::dispatch(
                session,
                Request::Single(Op {
                    c: protocol::op::LS,
                    ..Default::default()
                }),
            )
            .await;

            let idx_stats = match &session.index {
                Some(idx) => {
                    let stats = idx.stats().await;
                    Some(serde_json::to_value(stats).unwrap_or_default())
                }
                None => None,
            };

            let mut info = json!({});
            if let Some(d) = session_resp.d {
                info["session"] = d;
            }
            if let Some(d) = ls_resp.d {
                info["root_listing"] = d;
            }
            if let Some(stats) = idx_stats {
                info["index"] = stats;
            }

            response_to_result(Response::ok(info))
        }

        "exec" => {
            let resp = ops::dispatch(
                session,
                Request::Single(Op {
                    c: protocol::op::EXEC,
                    s: get_str(args, "command"),
                    a: get_str_array(args, "args"),
                    q: get_str(args, "cwd"),
                    kv: get_str_map(args, "env"),
                    ..Default::default()
                }),
            )
            .await;
            response_to_result(resp)
        }

        "tool_pipeline" => {
            let resp = ops::dispatch(
                session,
                Request::Single(Op {
                    c: protocol::op::TOOL_PIPELINE,
                    p: get_str(args, "tool_id"),
                    a: get_str_array(args, "stages"),
                    q: get_str(args, "cwd"),
                    ..Default::default()
                }),
            )
            .await;
            response_to_result(resp)
        }

        "tool_repair" => {
            let resp = ops::dispatch(
                session,
                Request::Single(Op {
                    c: protocol::op::TOOL_REPAIR,
                    p: get_str(args, "tool_id"),
                    n: get_i64(args, "max_iterations"),
                    q: get_str(args, "cwd"),
                    ..Default::default()
                }),
            )
            .await;
            response_to_result(resp)
        }

        "diff_files" => {
            let resp = ops::dispatch(
                session,
                Request::Single(Op {
                    c: protocol::op::DIFF,
                    p: get_str(args, "path_a"),
                    q: get_str(args, "path_b"),
                    s: get_str(args, "content_b"),
                    ..Default::default()
                }),
            )
            .await;
            response_to_result(resp)
        }

        "snapshot_create" => {
            let resp = ops::dispatch(
                session,
                Request::Single(Op {
                    c: protocol::op::SNAP,
                    p: get_str(args, "tag"),
                    ..Default::default()
                }),
            )
            .await;
            response_to_result(resp)
        }

        "snapshot_restore" => {
            let resp = ops::dispatch(
                session,
                Request::Single(Op {
                    c: protocol::op::RESTORE,
                    p: get_str(args, "id"),
                    ..Default::default()
                }),
            )
            .await;
            response_to_result(resp)
        }

        "snapshot_list" => {
            let resp = ops::dispatch(
                session,
                Request::Single(Op {
                    c: protocol::op::SNAP_LIST,
                    ..Default::default()
                }),
            )
            .await;
            response_to_result(resp)
        }

        "snapshot_delete" => {
            let resp = ops::dispatch(
                session,
                Request::Single(Op {
                    c: protocol::op::SNAP_DELETE,
                    p: get_str(args, "id"),
                    ..Default::default()
                }),
            )
            .await;
            response_to_result(resp)
        }

        tool if tool.starts_with("git_") => {
            let registry = match &session.tool_registry {
                Some(r) => r,
                None => return err_text("tool registry not available".into()),
            };

            let command = tool.strip_prefix("git_").unwrap();
            let cwd = session.cwd.clone();
            let env = session.env.clone();

            let extra = if !args.is_null() {
                Some(args.clone())
            } else {
                None
            };
            let extra_ref = extra.as_ref();

            match registry
                .run("git", command, &cwd, &env, None, extra_ref)
                .await
            {
                Ok(result) => {
                    let text = serde_json::to_string(&result.output).unwrap_or_default();
                    ok_text(text)
                }
                Err(e) => err_text(format!("git {command}: {e}")),
            }
        }

        _ => Err(CallToolError::unknown_tool(name.to_string())),
    }
}

fn extract_result_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| {
            if let rust_mcp_sdk::schema::ContentBlock::TextContent(tc) = c {
                Some(tc.text.clone())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("")
}

#[async_trait]
impl ServerHandler for DaimonosHandler {
    async fn handle_list_tools_request(
        &self,
        _request: Option<PaginatedRequestParams>,
        _runtime: Arc<dyn McpServer>,
    ) -> std::result::Result<ListToolsResult, RpcError> {
        let session = self.session.lock().await;
        let all = tool_definitions();
        let visible: Vec<Tool> = all
            .into_iter()
            .filter(|t| session.exposed_tools.contains(&t.name))
            .collect();
        Ok(ListToolsResult {
            tools: visible,
            meta: None,
            next_cursor: None,
        })
    }

    async fn handle_call_tool_request(
        &self,
        params: CallToolRequestParams,
        _runtime: Arc<dyn McpServer>,
    ) -> std::result::Result<CallToolResult, CallToolError> {
        let args: Value = serde_json::to_value(&params.arguments).unwrap_or(Value::Null);

        let mut session = self.session.lock().await;

        if params.name == "batch" {
            let ops = match args.get("ops").and_then(|v| v.as_array()) {
                Some(arr) => arr.clone(),
                None => return err_text("batch requires 'ops' array".into()),
            };

            let mut results = Vec::with_capacity(ops.len());
            for (i, op_val) in ops.iter().enumerate() {
                let tool = match op_val.get("tool").and_then(|v| v.as_str()) {
                    Some(t) => t.to_string(),
                    None => {
                        results.push(json!({"ok": false, "error": format!("ops[{i}]: missing 'tool' field")}));
                        continue;
                    }
                };

                if tool == "batch" {
                    results.push(
                        json!({"ok": false, "tool": "batch", "error": "nested batch not allowed"}),
                    );
                    continue;
                }

                let sub_args = op_val.get("arguments").cloned().unwrap_or(json!({}));

                match dispatch_tool(&mut session, &tool, &sub_args).await {
                    Ok(result) => {
                        let text = extract_result_text(&result);
                        let is_err = result.is_error.unwrap_or(false);
                        if is_err {
                            results.push(json!({"ok": false, "tool": tool, "error": text}));
                        } else {
                            let parsed: Value = serde_json::from_str(&text).unwrap_or(json!(text));
                            results.push(json!({"ok": true, "tool": tool, "data": parsed}));
                        }
                    }
                    Err(e) => {
                        results.push(json!({"ok": false, "tool": tool, "error": format!("{e:?}")}));
                    }
                }
            }

            return ok_text(serde_json::to_string(&results).unwrap_or_default());
        }

        dispatch_tool(&mut session, &params.name, &args).await
    }
}

// --- Tool definitions ---

fn tool_definitions() -> Vec<Tool> {
    let defs = vec![
        json!({
            "name": "read_file",
            "description": "Read file. Returns {content, lines} or {unchanged:true, lines} if already read and unmodified.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative path"},
                    "offset": {"type": "integer", "description": "Start line (0-based)"},
                    "limit": {"type": "integer", "description": "Max lines"}
                },
                "required": ["path"]
            }
        }),
        json!({
            "name": "write_file",
            "description": "Write file, creating parent dirs.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative path"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }
        }),
        json!({
            "name": "edit_file",
            "description": "String-replace edits. Returns {applied, diffs} confirming each change.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative path"},
                    "edits": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "[old, new, old, new, ...] pairs"
                    }
                },
                "required": ["path", "edits"]
            }
        }),
        json!({
            "name": "search",
            "description": "Regex content search or trigram file-name search.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "mode": {"type": "string", "enum": ["content", "files"], "description": "Default: content"},
                    "path": {"type": "string", "description": "Scope dir"},
                    "glob": {"type": "string", "description": "e.g. *.rs"},
                    "max_results": {"type": "integer"}
                },
                "required": ["pattern"]
            }
        }),
        json!({
            "name": "workspace_info",
            "description": "Session, root listing, and index stats in one call.",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        json!({
            "name": "exec",
            "description": "Run command. Returns {exit, out, err?}. Output auto-truncated if very large.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "args": {"type": "array", "items": {"type": "string"}},
                    "cwd": {"type": "string"},
                    "env": {"type": "object", "additionalProperties": {"type": "string"}}
                },
                "required": ["command"]
            }
        }),
        json!({
            "name": "tool_pipeline",
            "description": "Run tool stages sequentially, abort on failure.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tool_id": {"type": "string"},
                    "stages": {"type": "array", "items": {"type": "string"}},
                    "cwd": {"type": "string"}
                },
                "required": ["tool_id", "stages"]
            }
        }),
        json!({
            "name": "tool_repair",
            "description": "Auto lint-fix loop until clean.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tool_id": {"type": "string"},
                    "max_iterations": {"type": "integer", "description": "Default: 3"},
                    "cwd": {"type": "string"}
                },
                "required": ["tool_id"]
            }
        }),
        json!({
            "name": "snapshot_create",
            "description": "Snapshot workspace for rollback.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tag": {"type": "string", "description": "Optional label"}
                }
            }
        }),
        json!({
            "name": "snapshot_restore",
            "description": "Restore workspace from snapshot.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string"}
                },
                "required": ["id"]
            }
        }),
        json!({
            "name": "snapshot_list",
            "description": "List snapshots, newest first.",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        json!({
            "name": "snapshot_delete",
            "description": "Delete a snapshot.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string"}
                },
                "required": ["id"]
            }
        }),
        json!({
            "name": "diff_files",
            "description": "Structured diff: hunks with =, +, - tagged lines.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path_a": {"type": "string"},
                    "path_b": {"type": "string"},
                    "content_b": {"type": "string", "description": "Alt: diff against string"}
                },
                "required": ["path_a"]
            }
        }),
        json!({
            "name": "git_status",
            "description": "Structured working-tree status arrays.",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        json!({
            "name": "git_log",
            "description": "Commits as {hash, author, message, date}.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "description": "Default: 10"},
                    "path": {"type": "string", "description": "Filter by path"}
                }
            }
        }),
        json!({
            "name": "git_diff",
            "description": "Structured diff with files and hunks.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mode": {"type": "string", "enum": ["unstaged", "staged"]}
                }
            }
        }),
        json!({
            "name": "git_branch",
            "description": "Current branch and branch list.",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        json!({
            "name": "list_all_tools",
            "description": "Show all available tools including extended ones (snapshots, git, diff, pipelines). Call once to unlock them.",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        json!({
            "name": "batch",
            "description": "Multiple tools in one round-trip. Returns results array. Use for independent parallel ops.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ops": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "tool": {"type": "string"},
                                "arguments": {"type": "object"}
                            },
                            "required": ["tool"]
                        }
                    }
                },
                "required": ["ops"]
            }
        }),
    ];

    defs.into_iter()
        .map(|v| serde_json::from_value(v).expect("valid tool definition"))
        .collect()
}

// --- Proactive workspace context ---

/// Build dynamic instructions that include workspace-specific context
/// so the model has useful information without a separate tool call.
async fn build_instructions(workspace: &std::path::Path) -> String {
    let mut parts = vec![
        // Directive: tell the model to prefer daimonos tools
        "IMPORTANT: Always use daimonos tools instead of built-in equivalents. \
         Daimonos tools are faster, return structured JSON, and cost fewer tokens.\n\
         - read_file instead of Read/cat\n\
         - write_file instead of Write\n\
         - edit_file instead of StrReplace/Edit\n\
         - search instead of Grep/Glob/find\n\
         - exec instead of Shell\n\
         - batch to combine multiple operations in one call\n\
         Use list_all_tools to discover git, snapshot, and diff tools."
            .to_string(),
        format!("Workspace: {}", workspace.display()),
    ];

    // Detect primary language / project type from manifest files
    let markers: &[(&str, &str)] = &[
        ("Cargo.toml", "Rust (Cargo)"),
        ("package.json", "Node.js"),
        ("pyproject.toml", "Python"),
        ("go.mod", "Go"),
        ("Gemfile", "Ruby"),
        ("pom.xml", "Java (Maven)"),
        ("build.gradle", "Java/Kotlin (Gradle)"),
        ("CMakeLists.txt", "C/C++ (CMake)"),
        ("Makefile", "Make"),
    ];
    let mut detected: Vec<&str> = Vec::new();
    for (file, lang) in markers {
        if workspace.join(file).exists() {
            detected.push(lang);
        }
    }
    if !detected.is_empty() {
        parts.push(format!("Project type: {}", detected.join(", ")));
    }

    // Detect VCS
    if workspace.join(".git").exists() {
        parts.push("VCS: git".to_string());
    }

    // Top-level directory listing (dirs only, max 15)
    if let Ok(mut entries) = tokio::fs::read_dir(workspace).await {
        let mut dirs: Vec<String> = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Ok(ft) = entry.file_type().await {
                if ft.is_dir() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !name.starts_with('.') && name != "target" && name != "node_modules" {
                        dirs.push(name);
                    }
                }
            }
            if dirs.len() >= 15 {
                break;
            }
        }
        if !dirs.is_empty() {
            dirs.sort();
            parts.push(format!("Top-level dirs: {}", dirs.join(", ")));
        }
    }

    parts.join("\n")
}

// --- MCP server entry point ---

pub async fn run_mcp_server(
    workspace: PathBuf,
    cfg: Arc<Config>,
    ws_index: Arc<WorkspaceIndex>,
    tool_reg: Arc<ToolRegistry>,
    pcache: Arc<PipelineCache>,
) -> anyhow::Result<()> {
    let mut session = Session::new(workspace.clone(), cfg);
    session.index = Some(ws_index);
    session.tool_registry = Some(tool_reg);
    session.pipeline_cache = Some(pcache);

    let instructions = build_instructions(&workspace).await;
    let handler = DaimonosHandler::new(session);

    let server_details = InitializeResult {
        server_info: Implementation {
            name: "daimonos".into(),
            version: "0.1.0".into(),
            title: Some("Daimonos".into()),
            description: Some(
                "Agent-optimized OS layer with structured file, exec, and search operations".into(),
            ),
            icons: vec![],
            website_url: None,
        },
        capabilities: ServerCapabilities {
            tools: Some(ServerCapabilitiesTools {
                list_changed: Some(true),
            }),
            ..Default::default()
        },
        protocol_version: ProtocolVersion::V2025_11_25.into(),
        instructions: Some(instructions),
        meta: None,
    };

    let transport = StdioTransport::new(TransportOptions::default())
        .map_err(|e| anyhow::anyhow!("transport: {e}"))?;

    let options = McpServerOptions {
        server_details,
        transport,
        handler: handler.to_mcp_server_handler(),
        task_store: None,
        client_task_store: None,
        message_observer: None,
    };

    let server = server_runtime::create_server(options);

    eprintln!(
        "daimonos MCP server starting (stdio, workspace: {:?})",
        workspace
    );

    server
        .start()
        .await
        .map_err(|e| anyhow::anyhow!("mcp server: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_str_extracts_string() {
        let v = json!({"path": "/tmp/foo", "count": 5});
        assert_eq!(get_str(&v, "path"), Some("/tmp/foo".into()));
        assert_eq!(get_str(&v, "missing"), None);
        assert_eq!(get_str(&v, "count"), None); // not a string
    }

    #[test]
    fn get_i64_extracts_int() {
        let v = json!({"offset": 42, "name": "hi"});
        assert_eq!(get_i64(&v, "offset"), Some(42));
        assert_eq!(get_i64(&v, "missing"), None);
        assert_eq!(get_i64(&v, "name"), None); // not an int
    }

    #[test]
    fn get_str_array_extracts_vec() {
        let v = json!({"stages": ["build", "test", "deploy"]});
        let arr = get_str_array(&v, "stages").unwrap();
        assert_eq!(arr, vec!["build", "test", "deploy"]);
        assert!(get_str_array(&v, "missing").is_none());
    }

    #[test]
    fn get_str_array_returns_none_for_mixed() {
        let v = json!({"arr": ["ok", 5]});
        assert!(get_str_array(&v, "arr").is_none());
    }

    #[test]
    fn get_str_map_extracts_hashmap() {
        let v = json!({"env": {"KEY": "val", "FOO": "bar"}});
        let map = get_str_map(&v, "env").unwrap();
        assert_eq!(map.get("KEY"), Some(&"val".to_string()));
        assert_eq!(map.get("FOO"), Some(&"bar".to_string()));
    }

    #[test]
    fn get_str_map_skips_non_string_values() {
        let v = json!({"env": {"K": "v", "N": 5}});
        let map = get_str_map(&v, "env").unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("K"), Some(&"v".to_string()));
    }

    #[test]
    fn response_to_result_ok() {
        let resp = Response::ok(json!({"lines": 10}));
        let result = response_to_result(resp).unwrap();
        assert!(result.is_error.is_none() || !result.is_error.unwrap());
    }

    #[test]
    fn response_to_result_error() {
        let resp = Response::err(3, "bad path");
        let result = response_to_result(resp).unwrap();
        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn tool_definitions_has_entries() {
        let tools = tool_definitions();
        assert!(!tools.is_empty());
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
        assert!(names.contains(&"edit_file"));
        assert!(names.contains(&"exec"));
        assert!(names.contains(&"search"));
        assert!(names.contains(&"batch"));
        assert!(names.contains(&"list_all_tools"));
    }

    #[test]
    fn tool_definitions_all_have_descriptions() {
        for tool in tool_definitions() {
            assert!(!tool.name.is_empty(), "tool has empty name");
            assert!(
                tool.description.is_some() && !tool.description.as_ref().unwrap().is_empty(),
                "tool '{}' has no description",
                tool.name
            );
        }
    }

    #[test]
    fn tool_definitions_no_duplicates() {
        let tools = tool_definitions();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        let unique: std::collections::HashSet<&&str> = names.iter().collect();
        assert_eq!(names.len(), unique.len(), "duplicate tool names found");
    }

    #[tokio::test]
    async fn build_instructions_includes_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let instructions = build_instructions(dir.path()).await;
        let workspace_str = dir.path().to_string_lossy();
        assert!(
            instructions.contains(&*workspace_str),
            "instructions should contain workspace path"
        );
    }

    #[tokio::test]
    async fn build_instructions_detects_cargo() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("Cargo.toml"), "[package]")
            .await
            .unwrap();
        let instructions = build_instructions(dir.path()).await;
        assert!(instructions.contains("Rust (Cargo)"));
    }

    #[tokio::test]
    async fn build_instructions_detects_git() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir(dir.path().join(".git"))
            .await
            .unwrap();
        let instructions = build_instructions(dir.path()).await;
        assert!(instructions.contains("VCS: git"));
    }

    #[tokio::test]
    async fn build_instructions_lists_dirs() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir(dir.path().join("src")).await.unwrap();
        tokio::fs::create_dir(dir.path().join("tests"))
            .await
            .unwrap();
        let instructions = build_instructions(dir.path()).await;
        assert!(instructions.contains("Top-level dirs:"));
        assert!(instructions.contains("src"));
    }
}
