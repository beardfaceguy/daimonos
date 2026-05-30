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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

use crate::analytics::{self, AnalyticsStore, ToolCallRecord};
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
    /// Updated on every incoming request so the idle watchdog can detect
    /// abandonment. Storing unix-seconds in an `AtomicU64` lets the
    /// watchdog read it without ever blocking on the session mutex.
    last_activity: Arc<AtomicU64>,
}

impl DaimonosHandler {
    pub fn new(session: Session, last_activity: Arc<AtomicU64>) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            last_activity,
        }
    }

    fn poke_activity(&self) {
        self.last_activity.store(now_unix_secs(), Ordering::Relaxed);
    }
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
    let start = std::time::Instant::now();
    let request_chars = serde_json::to_string(args).map(|s| s.len()).unwrap_or(0);

    // Drain any stale meta from a prior call so `dispatch_tool_inner`
    // starts with a clean slate. Without this, a handler that doesn't set
    // meta (and there are several) could inherit flags from the previous
    // call on the same session.
    let _ = std::mem::take(&mut session.last_response_meta);

    let result = dispatch_tool_inner(session, name, args).await;

    // Always drain after the inner runs so the slot is reset for the next
    // turn — even when analytics is disabled. Reading the structured meta
    // here avoids the brittle wire-text inspection we used to do (substring
    // match → re-parse JSON → top-level key probe). Both of those approaches
    // were coupled to the response format and prone to drift; this reads
    // exactly what handlers set.
    let meta = std::mem::take(&mut session.last_response_meta);

    // Record analytics
    if let Some(analytics) = &session.analytics {
        let elapsed_ms = start.elapsed().as_millis() as u64;
        let (response_chars, was_redirect, was_filtered, read_dedup) = match &result {
            Ok(r) => (
                extract_result_text(r).len(),
                meta.redirect_via_plugin,
                meta.filter_applied,
                meta.read_dedup,
            ),
            Err(_) => (0, false, false, false),
        };

        let command = match name {
            "exec" | "git" | "cargo" | "gh" | "docker" => tools::get_str(args, "command"),
            "discord" => {
                let base = tools::get_str(args, "command");
                let tag = tools::get_str(args, "analytics_tag");
                match (base, tag) {
                    (Some(cmd), Some(t)) if !t.trim().is_empty() => {
                        Some(format!("{cmd}:{}", t.trim()))
                    }
                    (Some(cmd), _) => Some(cmd),
                    _ => None,
                }
            }
            _ => None,
        };

        let record = ToolCallRecord {
            tool_name: name.to_string(),
            command,
            request_tokens: analytics::estimate_tokens(request_chars),
            response_tokens: analytics::estimate_tokens(response_chars),
            saved_tokens: 0, // baseline comparison not available at this layer
            savings_pct: 0.0,
            exec_time_ms: elapsed_ms,
            was_redirect,
            was_filtered,
            read_dedup,
            batch_size: 1,
            external_session_id: session.external_session_id.clone(),
        };

        // Use the tracked spawn so the idle watchdog can drain in-flight
        // SQLite writes before exiting. A bare `spawn_blocking` would be
        // dropped on `std::process::exit` and silently lose the last few
        // tool calls of a session.
        analytics.record_async(record);
    }

    result
}

async fn dispatch_tool_inner(
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
                // Stash structured response metadata for the analytics layer.
                // Doing this here (rather than substring-matching the wire
                // text in `dispatch_tool`) avoids brittle false positives
                // when tool output happens to contain `"via":"plugin"` or
                // `"unchanged":true` as user data.
                session.last_response_meta = resp.meta.clone();
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
                    None => {
                        return err_text(
                            "get_tool_schema requires 'tools' array or 'tool' string".into(),
                        )
                    }
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
                err_text(format!(
                    "unknown tool(s): {:?}. Available: {:?}",
                    names, known
                ))
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

        "session_stats" => {
            let analytics = match &session.analytics {
                Some(a) => a,
                None => return err_text("analytics not enabled".into()),
            };

            let scope = tools::get_str(args, "scope").unwrap_or_else(|| "session".into());
            let days = tools::get_i64(args, "days").unwrap_or(30) as u64;
            let external_filter = tools::get_str(args, "external_session_id")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());

            match scope.as_str() {
                "session" => {
                    // Wrap the in-memory SessionStats in a JSON object that
                    // also surfaces the live external_session_id so the
                    // caller can confirm what `set_external_session_id` /
                    // the env-var bootstrap actually attached.
                    let stats = analytics.session_summary();
                    let mut value = serde_json::to_value(&stats).unwrap_or_default();
                    if let Some(obj) = value.as_object_mut() {
                        obj.insert(
                            "external_session_id".to_string(),
                            match &session.external_session_id {
                                Some(id) => json!(id),
                                None => Value::Null,
                            },
                        );
                    }
                    ok_text(serde_json::to_string(&value).unwrap_or_default())
                }
                "history" => {
                    match analytics.history_summary_filtered(days, external_filter.as_deref()) {
                        Ok(summary) => ok_text(serde_json::to_string(&summary).unwrap_or_default()),
                        Err(e) => err_text(format!("history query: {e}")),
                    }
                }
                "daily" => match analytics.daily_trend_filtered(days, external_filter.as_deref()) {
                    Ok(trend) => ok_text(serde_json::to_string(&trend).unwrap_or_default()),
                    Err(e) => err_text(format!("daily query: {e}")),
                },
                _ => err_text(format!(
                    "unknown scope: {scope}. Use session, history, or daily"
                )),
            }
        }

        "set_external_session_id" => {
            let id = match get_str(args, "id") {
                Some(s) => s,
                None => return err_text("set_external_session_id requires 'id' argument".into()),
            };
            let trimmed = id.trim().to_string();
            let previous = session.external_session_id.clone();
            session.external_session_id = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.clone())
            };
            ok_text(
                serde_json::to_string(&json!({
                    "external_session_id": session.external_session_id,
                    "previous": previous,
                }))
                .unwrap_or_default(),
            )
        }

        "workspace_info" => {
            use crate::protocol::{op, Op, Request};

            let session_resp = ops::dispatch(
                session,
                Request::Single(Op {
                    c: op::SESSION,
                    ..Default::default()
                }),
            )
            .await;

            let ls_resp = ops::dispatch(
                session,
                Request::Single(Op {
                    c: op::LS,
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

            let external_session_id = session.external_session_id.clone();
            let analytics_summary = session.analytics.as_ref().map(|a| {
                let s = a.session_summary();
                let mut j = json!({
                    "calls": s.total_calls,
                    "req_tokens": s.total_request_tokens,
                    "resp_tokens": s.total_response_tokens,
                    "saved_tokens": s.total_saved_tokens,
                    "redirects": s.redirect_hits,
                    "filters": s.filter_hits,
                    "dedup_hits": s.dedup_hits,
                });
                if let Some(p) = a.db_path() {
                    if let Some(obj) = j.as_object_mut() {
                        obj.insert("db_path".to_string(), json!(p.to_string_lossy()));
                    }
                }
                if let Some(id) = external_session_id {
                    if let Some(obj) = j.as_object_mut() {
                        obj.insert("external_session_id".to_string(), json!(id));
                    }
                }
                j
            });

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
            if let Some(a) = analytics_summary {
                info["analytics"] = a;
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

            // Canonicalize first, then stat the canonical path. Reversing
            // these (is_dir → canonicalize) opens a TOCTOU window where
            // a symlink target can be swapped between the two syscalls
            // and produces misleading errors for non-existent paths.
            let canonical = match new_cwd.canonicalize() {
                Ok(p) => p,
                Err(e) => return err_text(format!("resolve path: {e}")),
            };

            if !canonical.is_dir() {
                return err_text(format!("not a directory: {}", canonical.display()));
            }

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

        "discord" => {
            let command = match get_str(args, "command") {
                Some(c) => c,
                None => return err_text("discord requires 'command' argument".into()),
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
                .run("discord", &command, &cwd, &env, None, extra_ref)
                .await
            {
                Ok(result) => {
                    let text = serde_json::to_string(&result.output).unwrap_or_default();
                    ok_text(text)
                }
                Err(e) => err_text(format!("discord {command}: {e}")),
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
        self.poke_activity();
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
        self.poke_activity();
        let args: Value = serde_json::to_value(&params.arguments).unwrap_or(Value::Null);

        // execute_script needs Arc<Mutex<Session>> — handle it before locking.
        if params.name == "execute_script" {
            self.session
                .lock()
                .await
                .used_tools
                .insert("execute_script".into());
            let code = match args.get("code").and_then(|v| v.as_str()) {
                Some(c) => c.to_string(),
                None => return err_text("execute_script requires 'code' argument".into()),
            };
            let timeout_secs = args.get("timeout").and_then(|v| v.as_i64()).unwrap_or(60) as u64;
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
                Err(e) => err_text(format!("{e}")),
            };
        }

        // kgl_query opens the per-workspace KGL store; only needs the workspace path.
        if params.name == "kgl_query" {
            let action = args
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let qargs = args.get("args").cloned().unwrap_or_else(|| json!({}));
            let workspace = {
                let mut s = self.session.lock().await;
                s.used_tools.insert("kgl_query".into());
                s.workspace.clone()
            };
            let now = chrono::Utc::now().to_rfc3339();
            return match crate::kgl::query::run(&workspace, &action, &qargs, &now) {
                Ok(v) => ok_text(serde_json::to_string(&v).unwrap_or_default()),
                Err(e) => err_text(format!("{e}")),
            };
        }

        // kgl_assert: agent write path for the non-derivable intent/provenance layer.
        if params.name == "kgl_assert" {
            let action = args
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let aargs = args.get("args").cloned().unwrap_or_else(|| json!({}));
            let workspace = {
                let mut s = self.session.lock().await;
                s.used_tools.insert("kgl_assert".into());
                s.workspace.clone()
            };
            let now = chrono::Utc::now().to_rfc3339();
            return match crate::kgl::assert::run(&workspace, &action, &aargs, &now) {
                Ok(v) => ok_text(serde_json::to_string(&v).unwrap_or_default()),
                Err(e) => err_text(format!("{e}")),
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

        let result = dispatch_tool(&mut session, &params.name, &args).await;
        // Observed-provenance capture (KGL), gated off by default. Records direct
        // file ops as observed reads/mutates edges from the session. Best-effort:
        // never affects the tool result.
        if crate::kgl::observe::enabled() {
            if let Ok(r) = &result {
                if !r.is_error.unwrap_or(false) {
                    let now = chrono::Utc::now().to_rfc3339();
                    let sid = session
                        .external_session_id
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string());
                    let ws = session.workspace.clone();
                    let _ =
                        crate::kgl::observe::record_file_op(&ws, &sid, &params.name, &args, &now);
                }
            }
        }
        result
    }
}

// --- Proactive workspace context ---

/// Build dynamic instructions that include workspace-specific context
/// so the model has useful information without a separate tool call.
/// Orientation hint nudging agents to query the KGL graph first (and to record
/// intent as they work). Only emitted when KGL auto-indexing is on, so the
/// graph actually exists. Pure (takes the gate) so it's testable without env.
fn kgl_instructions_hint(kgl_enabled: bool) -> Option<&'static str> {
    if !kgl_enabled {
        return None;
    }
    Some(
        "KGL graph: this workspace has a queryable code+intent knowledge graph. To orient before \
         reading source, call kgl_query {query:'orient', args:{task:'<topic>'}} — one call returns \
         matching defs + their intent/open-questions + edges + dependents. Record intent / \
         provenance / contracts as you work with kgl_assert. Reading source still finds latent \
         issues the graph hasn't been told about.",
    )
}

async fn build_instructions(workspace: &std::path::Path) -> String {
    let mut parts = vec![
        "Use daimonos tools, not built-in equivalents.".to_string(),
        "If your plan requires 2+ tool calls, use execute_script instead — write a Starlark script that calls the tool functions and sets `result`. This is faster and cheaper than sequential calls. Only call individual tools when you need exactly one operation.".to_string(),
        "Terse output. Drop filler, articles, pleasantries, hedging. Fragments OK. Technical substance exact. Code unchanged. Pattern: [thing] [action] [reason].".to_string(),
        format!("Workspace: {}", workspace.display()),
    ];

    parts.push(format!(
        "Starlark tool functions for execute_script:\n{}",
        script::tool_signatures()
    ));

    if let Some(hint) = kgl_instructions_hint(crate::kgl::autoindex::enabled()) {
        parts.push(hint.to_string());
    }

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

/// Resolve the effective idle timeout in seconds. The
/// `DAIMONOS_IDLE_TIMEOUT_SECS` environment variable wins over the
/// `[mcp] idle_timeout_secs` config value when present and parseable;
/// this lets the test suite use very short timeouts without writing a
/// config file. A value of `0` from either source disables the watchdog.
fn effective_idle_timeout(cfg: &Config) -> u64 {
    if let Ok(raw) = std::env::var("DAIMONOS_IDLE_TIMEOUT_SECS") {
        if let Ok(parsed) = raw.trim().parse::<u64>() {
            return parsed;
        }
    }
    cfg.mcp.idle_timeout_secs
}

/// Spawn a background tokio task that exits the process when the MCP
/// server has been idle (no incoming list_tools / call_tool requests) for
/// longer than `timeout_secs`. The watchdog is disabled when
/// `timeout_secs == 0`.
///
/// This protects against the leaked-stdin scenario: a parent editor
/// closes its agent panel without sending a shutdown and without closing
/// the stdin pipe (because another worker still holds the write-end).
/// The MCP read loop would otherwise block forever, leaving an orphan
/// daimonos process holding inotify watches, fds and memory.
fn spawn_idle_watchdog(
    timeout_secs: u64,
    last_activity: Arc<AtomicU64>,
    analytics: Option<Arc<AnalyticsStore>>,
    startup_logs: bool,
) {
    if timeout_secs == 0 {
        if startup_logs {
            eprintln!("daimonos: idle watchdog disabled (idle_timeout_secs = 0)");
        }
        return;
    }
    if startup_logs {
        eprintln!(
            "daimonos: idle watchdog armed ({timeout_secs}s); process will exit after that long without an MCP request"
        );
    }
    tokio::spawn(async move {
        // Tick frequently enough that the test suite can use short
        // timeouts (e.g. 2s) without flake, but not so often that the
        // watchdog wastes CPU on a quiet server.
        let interval = Duration::from_millis(500);
        loop {
            tokio::time::sleep(interval).await;
            let last = last_activity.load(Ordering::Relaxed);
            let now = now_unix_secs();
            let idle = now.saturating_sub(last);
            if idle >= timeout_secs {
                if startup_logs {
                    eprintln!(
                        "daimonos: idle for {idle}s (>= {timeout_secs}s timeout) — exiting to release resources"
                    );
                }
                // Drain any in-flight SQLite writes spawned by
                // `AnalyticsStore::record_async`. Without this gate the
                // process exits while the blocking pool still holds
                // unsent INSERTs and we silently lose the trailing tool
                // calls of the session — exactly the analytics we'd want
                // most when investigating idle-shutdown behavior.
                if let Some(an) = analytics.as_ref() {
                    let pending = an.pending_writes();
                    if pending > 0 {
                        if startup_logs {
                            eprintln!(
                                "daimonos: draining {pending} pending analytics writes before exit"
                            );
                        }
                        let drained = an.wait_until_quiet(Duration::from_secs(2)).await;
                        if !drained && startup_logs {
                            eprintln!(
                                "daimonos: analytics drain timed out; {} writes may be lost",
                                an.pending_writes()
                            );
                        }
                    }
                }
                std::process::exit(0);
            }
        }
    });
}

pub async fn run_mcp_server(
    workspace: PathBuf,
    cfg: Arc<Config>,
    ws_index: Arc<WorkspaceIndex>,
    tool_reg: Arc<ToolRegistry>,
    pcache: Arc<PipelineCache>,
    analytics: Option<Arc<AnalyticsStore>>,
    startup_logs: bool,
) -> anyhow::Result<()> {
    let idle_timeout_secs = effective_idle_timeout(&cfg);
    let last_activity = Arc::new(AtomicU64::new(now_unix_secs()));
    spawn_idle_watchdog(
        idle_timeout_secs,
        last_activity.clone(),
        analytics.clone(),
        startup_logs,
    );

    let mut session = Session::new(workspace.clone(), cfg);
    session.index = Some(ws_index);
    session.tool_registry = Some(tool_reg);
    session.pipeline_cache = Some(pcache);
    session.analytics = analytics;
    // Bootstrap the agent-runtime session id from the launch environment
    // so analytics rows can be correlated post-hoc with tools like
    // `claude --session-id $SID`. The MCP `set_external_session_id` tool
    // can override this mid-session.
    session.external_session_id = analytics::read_agent_session_id_env();

    let instructions = build_instructions(&workspace).await;
    let handler = DaimonosHandler::new(session, last_activity);

    let server_details = InitializeResult {
        server_info: Implementation {
            name: "daimonos".into(),
            version: env!("CARGO_PKG_VERSION").into(),
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

    if startup_logs {
        eprintln!(
            "daimonos MCP server starting (stdio, workspace: {:?})",
            workspace
        );
    }

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

    // --- ResponseMeta plumbing (vikunja #247): structured analytics signals ---
    //
    // Replaces the previous wire-text classifier (`classify_response`) and
    // the substring matcher before that. Handlers now set `Response.meta`
    // at the source site and the MCP layer reads those flags directly via
    // `session.last_response_meta`. This guarantees no false positives
    // from user-supplied content and no drift from the wire format.

    /// Drive the full plumbing: a real `Session` + a real `dispatch_tool`
    /// call for a `read_file` that produces a dedup hit must (a) stash
    /// `meta.read_dedup = true` via the inner dispatcher and then (b) have
    /// it consumed (`mem::take`) by the outer `dispatch_tool` so the slot
    /// is reset before the next turn.
    #[tokio::test]
    async fn dispatch_tool_threads_meta_through_session_and_resets_after() {
        use crate::config::Config;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dedup.txt"), "hello\n").unwrap();
        let mut session = Session::new(dir.path().to_path_buf(), Arc::new(Config::default()));

        let args = json!({"path": "dedup.txt"});

        // First read: cache miss. dispatch_tool consumes whatever the inner
        // produced and resets the slot to default.
        let _ = dispatch_tool(&mut session, "read_file", &args)
            .await
            .unwrap();
        assert_eq!(
            session.last_response_meta,
            crate::protocol::ResponseMeta::default(),
            "dispatch_tool must reset last_response_meta after every call"
        );

        // Second read: cache hit. The inner dispatcher must set
        // meta.read_dedup BEFORE dispatch_tool takes it; we observe the
        // reset side of the contract again, plus the unchanged payload.
        let result = dispatch_tool(&mut session, "read_file", &args)
            .await
            .unwrap();
        let payload = extract_result_text(&result);
        let parsed: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(
            parsed.get("unchanged"),
            Some(&Value::Bool(true)),
            "second read of unchanged file must dedup; payload = {payload}"
        );
        assert_eq!(
            session.last_response_meta,
            crate::protocol::ResponseMeta::default(),
            "dispatch_tool must reset last_response_meta after consuming the dedup signal"
        );
    }

    /// Inner-only test: skip `dispatch_tool`'s `mem::take` and assert that
    /// `dispatch_tool_inner` actually stashes `meta` on the session. Pinning
    /// this directly catches regressions where the stashing line is removed
    /// or moved out of the dispatch path.
    #[tokio::test]
    async fn dispatch_tool_inner_stashes_meta_on_session() {
        use crate::config::Config;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dedup.txt"), "hello\n").unwrap();
        let mut session = Session::new(dir.path().to_path_buf(), Arc::new(Config::default()));

        let args = json!({"path": "dedup.txt"});
        let _ = dispatch_tool_inner(&mut session, "read_file", &args)
            .await
            .unwrap();
        assert!(
            !session.last_response_meta.read_dedup,
            "first read is a cache miss; meta.read_dedup must be false"
        );

        let _ = dispatch_tool_inner(&mut session, "read_file", &args)
            .await
            .unwrap();
        assert!(
            session.last_response_meta.read_dedup,
            "second read of unchanged content must stash meta.read_dedup = true"
        );
        assert!(!session.last_response_meta.redirect_via_plugin);
        assert!(!session.last_response_meta.filter_applied);
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
        assert!(names.contains(&"discord"));
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
        let code_mode_reduction = 100.0 * (1.0 - code_mode_chars as f64 / full_chars as f64);

        eprintln!("=== Schema Token Benchmark ===");
        eprintln!("Full schema:      {full_chars:>5} chars ({full_tokens:>4} est. tokens) — {total} tools",
            total = all.len());
        eprintln!("Terse schema:     {terse_chars:>5} chars ({terse_tokens:>4} est. tokens) — {schema_reduction:.1}% reduction");
        eprintln!("Code-mode schema: {code_mode_chars:>5} chars ({code_mode_tokens:>4} est. tokens) — {code_mode_reduction:.1}% reduction");
        eprintln!(
            "Tool signatures:  {sig_chars:>5} chars ({sig_tokens:>4} est. tokens) — one-time cost"
        );
        eprintln!(
            "Code-mode per-turn: {:>4} est. tokens (schema) vs {:>4} full ({:.1}% saved)",
            code_mode_tokens, full_tokens, code_mode_reduction,
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

    #[test]
    fn kgl_hint_is_gated_and_mentions_orient() {
        assert!(kgl_instructions_hint(false).is_none());
        let hint = kgl_instructions_hint(true).expect("hint when enabled");
        assert!(hint.contains("orient"));
        assert!(hint.contains("kgl_assert"));
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

    // --- external_session_id correlation (vikunja #43) ---

    /// `set_external_session_id` must mutate the session field, surface
    /// the previous value, and treat an empty string as a clear.
    #[tokio::test]
    async fn set_external_session_id_updates_and_clears() {
        use crate::config::Config;

        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::new(dir.path().to_path_buf(), Arc::new(Config::default()));
        assert!(session.external_session_id.is_none());

        let result = dispatch_tool_inner(
            &mut session,
            "set_external_session_id",
            &json!({"id": "claude-sid-XYZ"}),
        )
        .await
        .unwrap();
        let payload: Value = serde_json::from_str(&extract_result_text(&result)).unwrap();
        assert_eq!(payload["external_session_id"], json!("claude-sid-XYZ"));
        assert_eq!(payload["previous"], Value::Null);
        assert_eq!(
            session.external_session_id.as_deref(),
            Some("claude-sid-XYZ")
        );

        let result =
            dispatch_tool_inner(&mut session, "set_external_session_id", &json!({"id": ""}))
                .await
                .unwrap();
        let payload: Value = serde_json::from_str(&extract_result_text(&result)).unwrap();
        assert_eq!(payload["external_session_id"], Value::Null);
        assert_eq!(payload["previous"], json!("claude-sid-XYZ"));
        assert!(session.external_session_id.is_none());
    }

    /// The `session_stats` session-scope response must echo the live
    /// `external_session_id` so callers can confirm what was attached.
    #[tokio::test]
    async fn session_stats_session_scope_includes_external_session_id() {
        use crate::config::Config;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let mut session = Session::new(dir.path().to_path_buf(), Arc::new(Config::default()));
        let analytics_dir = TempDir::new().unwrap();
        let analytics =
            Arc::new(AnalyticsStore::new(&analytics_dir.path().join("a.db"), 90).unwrap());
        session.analytics = Some(analytics);
        session.external_session_id = Some("agent-sid-123".to_string());

        let result =
            dispatch_tool_inner(&mut session, "session_stats", &json!({"scope": "session"}))
                .await
                .unwrap();
        let payload: Value = serde_json::from_str(&extract_result_text(&result)).unwrap();
        assert_eq!(payload["external_session_id"], json!("agent-sid-123"));
        assert!(payload.get("total_calls").is_some());
    }

    /// History/daily scopes must accept an `external_session_id` filter
    /// and forward it to the underlying SQL — verified end-to-end by
    /// recording rows under two different ids and asserting the filtered
    /// scope returns only the matching subset.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn session_stats_history_scope_filters_by_external_session_id() {
        use crate::config::Config;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let mut session = Session::new(dir.path().to_path_buf(), Arc::new(Config::default()));
        let analytics_dir = TempDir::new().unwrap();
        let analytics =
            Arc::new(AnalyticsStore::new(&analytics_dir.path().join("a.db"), 90).unwrap());

        let make_record = |tool: &str, ext: &str| ToolCallRecord {
            tool_name: tool.into(),
            command: None,
            request_tokens: 10,
            response_tokens: 5,
            saved_tokens: 0,
            savings_pct: 0.0,
            exec_time_ms: 1,
            was_redirect: false,
            was_filtered: false,
            read_dedup: false,
            batch_size: 1,
            external_session_id: Some(ext.to_string()),
        };
        analytics.record(&make_record("read_file", "sid-A"));
        analytics.record(&make_record("read_file", "sid-A"));
        analytics.record(&make_record("read_file", "sid-B"));

        session.analytics = Some(analytics);

        let result = dispatch_tool_inner(
            &mut session,
            "session_stats",
            &json!({"scope": "history", "external_session_id": "sid-A"}),
        )
        .await
        .unwrap();
        let payload: Value = serde_json::from_str(&extract_result_text(&result)).unwrap();
        assert_eq!(payload["total_calls"], json!(2));

        let result = dispatch_tool_inner(
            &mut session,
            "session_stats",
            &json!({"scope": "history", "external_session_id": "sid-B"}),
        )
        .await
        .unwrap();
        let payload: Value = serde_json::from_str(&extract_result_text(&result)).unwrap();
        assert_eq!(payload["total_calls"], json!(1));

        let result =
            dispatch_tool_inner(&mut session, "session_stats", &json!({"scope": "history"}))
                .await
                .unwrap();
        let payload: Value = serde_json::from_str(&extract_result_text(&result)).unwrap();
        assert_eq!(payload["total_calls"], json!(3));
    }
}
