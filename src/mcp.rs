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
use crate::protocol::Response;
use crate::script;
use crate::session::Session;
use crate::tool_runner::ToolRegistry;
use crate::tools;

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

use tools::get_str;

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
    if !session.exposed_tools.contains(name) {
        session.activate_tool(name);
    }
    session.used_tools.insert(name.to_string());

    // Registry-based dispatch: if the tool has a to_request mapping, use it
    if let Some(result) = tools::build_request(name, args) {
        match result {
            Ok(request) => {
                let resp = ops::dispatch(session, request).await;
                return response_to_result(resp);
            }
            Err(e) => return err_text(e),
        }
    }

    // Special tools that need session access or custom handling
    match name {
        "get_tool_schema" => {
            let names = match args.get("tools").and_then(|v| v.as_array()) {
                Some(arr) => arr
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>(),
                None => match get_str(args, "tool") {
                    Some(t) => vec![t],
                    None => return err_text("get_tool_schema requires 'tools' array or 'tool' string".into()),
                },
            };

            let all = tools::tool_definitions();
            let results: Vec<Value> = all
                .into_iter()
                .filter(|t| names.contains(&t.name))
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": t.input_schema,
                    })
                })
                .collect();

            if results.is_empty() {
                let known: Vec<&str> = tools::all_tool_names();
                err_text(format!("unknown tool(s): {:?}. Available: {:?}", names, known))
            } else {
                ok_text(serde_json::to_string(&results).unwrap_or_default())
            }
        }

        "list_tool_signatures" => {
            let sigs = script::tool_signatures();
            ok_text(sigs)
        }

        "list_all_tools" => {
            session.activate_all_tools();
            let all = tools::tool_definitions();
            let summary: Vec<Value> = all
                .iter()
                .map(|t| json!({"name": t.name, "description": t.description}))
                .collect();
            ok_text(serde_json::to_string(&summary).unwrap_or_default())
        }

        "workspace_info" => {
            use crate::protocol::{Op, Request, op};

            let session_resp = ops::dispatch(
                session,
                Request::Single(Op { c: op::SESSION, ..Default::default() }),
            )
            .await;

            let ls_resp = ops::dispatch(
                session,
                Request::Single(Op { c: op::LS, ..Default::default() }),
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

        "set_cwd" => {
            let path = match get_str(args, "path") {
                Some(p) => p,
                None => return err_text("set_cwd requires 'path' argument".into()),
            };

            let previous = session.cwd.display().to_string();
            let new_cwd = session.resolve_path(&path);

            if !new_cwd.is_dir() {
                return err_text(format!("not a directory: {}", new_cwd.display()));
            }

            let canonical = match new_cwd.canonicalize() {
                Ok(p) => p,
                Err(e) => return err_text(format!("resolve path: {e}")),
            };

            session.cwd = canonical.clone();
            ok_text(
                serde_json::to_string(&json!({
                    "cwd": canonical.display().to_string(),
                    "previous": previous,
                }))
                .unwrap_or_default(),
            )
        }

        "git" => {
            let command = match get_str(args, "command") {
                Some(c) => c,
                None => return err_text("git requires 'command' argument".into()),
            };

            let registry = match &session.tool_registry {
                Some(r) => r,
                None => return err_text("tool registry not available".into()),
            };

            let cwd = session.cwd.clone();
            let env = session.env.clone();

            let extra = if !args.is_null() {
                Some(args.clone())
            } else {
                None
            };
            let extra_ref = extra.as_ref();

            match registry
                .run("git", &command, &cwd, &env, None, extra_ref)
                .await
            {
                Ok(result) => {
                    let text = serde_json::to_string(&result.output).unwrap_or_default();
                    ok_text(text)
                }
                Err(e) => err_text(format!("git {command}: {e}")),
            }
        }

        "gh" => {
            let command = match get_str(args, "command") {
                Some(c) => c,
                None => return err_text("gh requires 'command' argument".into()),
            };

            let registry = match &session.tool_registry {
                Some(r) => r,
                None => return err_text("tool registry not available".into()),
            };

            let cwd = session.cwd.clone();
            let env = session.env.clone();

            let extra = if !args.is_null() {
                Some(args.clone())
            } else {
                None
            };
            let extra_ref = extra.as_ref();

            match registry
                .run("gh", &command, &cwd, &env, None, extra_ref)
                .await
            {
                Ok(result) => {
                    let text = serde_json::to_string(&result.output).unwrap_or_default();
                    ok_text(text)
                }
                Err(e) => err_text(format!("gh {command}: {e}")),
            }
        }

        "cargo" => {
            let command = match get_str(args, "command") {
                Some(c) => c,
                None => return err_text("cargo requires 'command' argument".into()),
            };

            let registry = match &session.tool_registry {
                Some(r) => r,
                None => return err_text("tool registry not available".into()),
            };

            let cwd = session.cwd.clone();
            let env = session.env.clone();

            let extra = if !args.is_null() {
                Some(args.clone())
            } else {
                None
            };
            let extra_ref = extra.as_ref();

            match registry
                .run("cargo", &command, &cwd, &env, None, extra_ref)
                .await
            {
                Ok(result) => {
                    let text = serde_json::to_string(&result.output).unwrap_or_default();
                    ok_text(text)
                }
                Err(e) => err_text(format!("cargo {command}: {e}")),
            }
        }

        "docker" => {
            let command = match get_str(args, "command") {
                Some(c) => c,
                None => return err_text("docker requires 'command' argument".into()),
            };

            let registry = match &session.tool_registry {
                Some(r) => r,
                None => return err_text("tool registry not available".into()),
            };

            let cwd = session.cwd.clone();
            let env = session.env.clone();

            let extra = if !args.is_null() {
                Some(args.clone())
            } else {
                None
            };
            let extra_ref = extra.as_ref();

            match registry
                .run("docker", &command, &cwd, &env, None, extra_ref)
                .await
            {
                Ok(result) => {
                    let text = serde_json::to_string(&result.output).unwrap_or_default();
                    ok_text(text)
                }
                Err(e) => err_text(format!("docker {command}: {e}")),
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
        let all = tools::tool_definitions();
        let workspace = &session.workspace;

        let visible: Vec<Tool> = all
            .into_iter()
            .filter(|t| session.exposed_tools.contains(&t.name))
            .filter(|t| tools::passes_context_check(&t.name, workspace))
            .map(|t| {
                let already_used = session.used_tools.contains(&t.name);
                if tools::has_full_schema(&t.name) && !already_used {
                    t
                } else {
                    Tool {
                        input_schema: serde_json::from_value(json!({"type": "object"}))
                            .unwrap_or(t.input_schema),
                        ..t
                    }
                }
            })
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

        // execute_script needs Arc<Mutex<Session>> — handle it before locking.
        if params.name == "execute_script" {
            self.session.lock().await.used_tools.insert("execute_script".into());
            let code = match args.get("code").and_then(|v| v.as_str()) {
                Some(c) => c.to_string(),
                None => return err_text("execute_script requires 'code' argument".into()),
            };
            let timeout_secs = args
                .get("timeout")
                .and_then(|v| v.as_i64())
                .unwrap_or(60) as u64;
            let timeout = std::time::Duration::from_secs(timeout_secs);

            return match script::execute(&code, self.session.clone(), timeout).await {
                Ok(result) => {
                    let mut resp = json!({
                        "result": result.value,
                    });
                    if !result.logs.is_empty() {
                        resp["logs"] = json!(result.logs);
                    }
                    ok_text(serde_json::to_string(&resp).unwrap_or_default())
                }
                Err(e) => err_text(format!("{e}"))
            };
        }

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

// --- Proactive workspace context ---

/// Build dynamic instructions that include workspace-specific context
/// so the model has useful information without a separate tool call.
async fn build_instructions(workspace: &std::path::Path) -> String {
    let mut parts = vec![
        "Use daimonos tools, not built-in equivalents.".to_string(),
        "If your plan requires 2+ tool calls, use execute_script instead — write a Starlark script that calls the tool functions and sets `result`. This is faster and cheaper than sequential calls. Only call individual tools when you need exactly one operation.".to_string(),
        "Terse output. Drop filler, articles, pleasantries, hedging. Fragments OK. Technical substance exact. Code unchanged. Pattern: [thing] [action] [reason].".to_string(),
        format!("Workspace: {}", workspace.display()),
    ];

    parts.push(format!("Starlark tool functions for execute_script:\n{}", script::tool_signatures()));

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
        let defs = tools::tool_definitions();
        assert!(!defs.is_empty());
        let names: Vec<&str> = defs.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
        assert!(names.contains(&"edit_file"));
        assert!(names.contains(&"exec"));
        assert!(names.contains(&"search"));
        assert!(names.contains(&"batch"));
        assert!(names.contains(&"list_all_tools"));
        assert!(names.contains(&"get_tool_schema"));
        assert!(names.contains(&"git"));
        assert!(names.contains(&"docker"));
        assert!(names.contains(&"snapshot"));
    }

    #[test]
    fn tool_definitions_all_have_descriptions() {
        for tool in tools::tool_definitions() {
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
        let defs = tools::tool_definitions();
        let names: Vec<&str> = defs.iter().map(|t| t.name.as_str()).collect();
        let unique: std::collections::HashSet<&&str> = names.iter().collect();
        assert_eq!(names.len(), unique.len(), "duplicate tool names found");
    }

    #[test]
    fn schema_token_savings_benchmark() {
        let all = tools::tool_definitions();

        let full_json: Vec<Value> = all
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema,
                })
            })
            .collect();
        let full_str = serde_json::to_string(&full_json).unwrap();
        let full_chars = full_str.len();

        let terse_json: Vec<Value> = all
            .iter()
            .map(|t| {
                if tools::has_full_schema(&t.name) {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": t.input_schema,
                    })
                } else {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": {"type": "object"},
                    })
                }
            })
            .collect();
        let terse_str = serde_json::to_string(&terse_json).unwrap();
        let terse_chars = terse_str.len();

        let code_mode_tools: Vec<Value> = all
            .iter()
            .filter(|t| {
                matches!(
                    t.name.as_str(),
                    "execute_script" | "list_tool_signatures" | "get_tool_schema"
                )
            })
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema,
                })
            })
            .collect();
        let code_mode_str = serde_json::to_string(&code_mode_tools).unwrap();
        let code_mode_chars = code_mode_str.len();

        let sigs = crate::script::tool_signatures();
        let sig_chars = sigs.len();

        let full_tokens = full_chars / 4;
        let terse_tokens = terse_chars / 4;
        let code_mode_tokens = code_mode_chars / 4;
        let sig_tokens = sig_chars / 4;

        let schema_reduction = 100.0 * (1.0 - terse_chars as f64 / full_chars as f64);
        let code_mode_reduction =
            100.0 * (1.0 - code_mode_chars as f64 / full_chars as f64);

        eprintln!("=== Schema Token Benchmark ===");
        eprintln!("Full schema:      {full_chars:>5} chars ({full_tokens:>4} est. tokens) — {total} tools",
            total = all.len());
        eprintln!("Terse schema:     {terse_chars:>5} chars ({terse_tokens:>4} est. tokens) — {schema_reduction:.1}% reduction");
        eprintln!("Code-mode schema: {code_mode_chars:>5} chars ({code_mode_tokens:>4} est. tokens) — {code_mode_reduction:.1}% reduction");
        eprintln!("Tool signatures:  {sig_chars:>5} chars ({sig_tokens:>4} est. tokens) — one-time cost");
        eprintln!(
            "Code-mode per-turn: {:>4} est. tokens (schema) vs {:>4} full ({:.1}% saved)",
            code_mode_tokens,
            full_tokens,
            code_mode_reduction,
        );

        assert!(
            schema_reduction > 20.0,
            "terse schema should save >20% tokens, got {schema_reduction:.1}%"
        );
        assert!(
            code_mode_reduction > 75.0,
            "code-mode should save >75% schema tokens, got {code_mode_reduction:.1}%"
        );
        assert!(
            code_mode_tools.len() == 3,
            "code-mode surface should be 3 tools"
        );
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
