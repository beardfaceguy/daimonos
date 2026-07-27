use async_trait::async_trait;
use rust_mcp_sdk::mcp_server::{
    server_runtime, McpServerOptions, ServerHandler, ToMcpServerHandler,
};
use rust_mcp_sdk::schema::{
    CallToolError, CallToolRequestParams, CallToolResult, Implementation, InitializeResult,
    ListToolsResult, PaginatedRequestParams, ProtocolVersion, Root, RpcError, ServerCapabilities,
    ServerCapabilitiesTools, TextContent, Tool,
};
use rust_mcp_sdk::{McpServer, StdioTransport, TransportOptions};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::analytics::{self, AnalyticsStore, ToolCallRecord};
use crate::config::{self, Config};
use crate::index::WorkspaceIndex;
use crate::ops;
use crate::pipeline_cache::PipelineCache;
use crate::protocol::{Response, ResponseMeta};
use crate::script;
use crate::session::Session;
use crate::snapshot::SnapshotStore;
use crate::tool_facade;
use crate::tool_runner::ToolRegistry;
use crate::tools;

pub(crate) const DAIMONOS_MCP_META_KEY: &str = "io.daimonos/server-kind";
pub(crate) const DAIMONOS_MCP_META_VALUE: &str = "mcp";

fn mcp_identity_meta() -> serde_json::Map<String, Value> {
    serde_json::Map::from_iter([(
        DAIMONOS_MCP_META_KEY.to_string(),
        Value::String(DAIMONOS_MCP_META_VALUE.to_string()),
    )])
}

/// Liveness signal shared by the request handlers and the idle watchdog.
///
/// Tracks two things, and the watchdog needs both:
///
/// * `last_activity` — unix-seconds of the most recent request boundary. Kept
///   in an atomic so the watchdog never blocks on the session mutex.
/// * `in_flight` — how many requests are currently executing.
///
/// `in_flight` exists because a timestamp alone cannot distinguish an
/// abandoned server from a busy one. A single `exec` running a long build, or
/// a command that hangs outright, produces no new requests, so a
/// timestamp-only watchdog counted it as idle and killed the process
/// mid-request. The client then saw an opaque transport error and lost the
/// call's result.
pub struct ActivityTracker {
    last_activity: AtomicU64,
    in_flight: AtomicUsize,
}

impl ActivityTracker {
    pub fn new() -> Self {
        Self {
            last_activity: AtomicU64::new(now_unix_secs()),
            in_flight: AtomicUsize::new(0),
        }
    }

    /// Stamp the clock without claiming an in-flight slot.
    fn poke(&self) {
        self.last_activity.store(now_unix_secs(), Ordering::Relaxed);
    }

    /// Mark a request as executing. The returned guard releases the slot and
    /// re-stamps the clock on drop, so the idle window is measured from when a
    /// request *finished* rather than when it arrived. Drop runs on early
    /// return and on unwind, so a failing handler cannot leak a slot and wedge
    /// the watchdog off forever.
    fn begin_request(self: &Arc<Self>) -> RequestGuard {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        self.poke();
        RequestGuard(Arc::clone(self))
    }

    fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }

    fn idle_secs(&self) -> u64 {
        now_unix_secs().saturating_sub(self.last_activity.load(Ordering::Relaxed))
    }

    /// Sample `(in_flight, idle_secs)` in the one order that is sound.
    ///
    /// The two values live in separate atomics, so any pair of reads is not a
    /// consistent snapshot, and the order is load-bearing. Reading the clock
    /// first admits this interleaving: read a stale timestamp, a long call then
    /// completes (refreshing the clock and releasing its slot), read
    /// `in_flight` as 0, and act on the stale reading — which defeats the
    /// poke-before-release order in [`RequestGuard::drop`] entirely.
    ///
    /// Reading `in_flight` first inverts that. The guard stores the timestamp
    /// and *then* releases the slot with a SeqCst read-modify-write, so a
    /// SeqCst load that observes the released count synchronizes-with that
    /// release and the timestamp read afterwards is guaranteed to be the
    /// refreshed one.
    ///
    /// Encapsulated here rather than done at the call site so the ordering
    /// cannot be silently reversed by a later edit.
    fn liveness_snapshot(&self) -> (usize, u64) {
        let in_flight = self.in_flight();
        let idle_secs = self.idle_secs();
        (in_flight, idle_secs)
    }
}

impl Default for ActivityTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Releases an in-flight slot when a request handler returns.
struct RequestGuard(Arc<ActivityTracker>);

impl Drop for RequestGuard {
    fn drop(&mut self) {
        // Order matters. Stamp the clock BEFORE releasing the slot.
        //
        // The watchdog vetoes on `in_flight > 0`, so the instant the count
        // reaches zero the timestamp must already be fresh. Releasing first
        // leaves a window where the watchdog sees an unoccupied server whose
        // clock is still stale by the entire duration of the call that just
        // finished — and for any call longer than the timeout that means it
        // exits immediately, reintroducing the very bug this guard exists to
        // prevent, just on completion instead of mid-call.
        //
        // The SeqCst read-modify-write below also publishes the store above,
        // but only for a reader that loads `in_flight` *before* the timestamp:
        // observing the released count then synchronizes-with this release. That
        // read order is why [`ActivityTracker::liveness_snapshot`] exists; a
        // reader that samples the clock first gets no such guarantee.
        self.0.poke();
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Whether the watchdog should terminate the process.
///
/// Split out from the timer loop so the policy is unit-testable. A non-zero
/// `in_flight` vetoes the exit no matter how stale the clock is: work in
/// progress is the strongest possible evidence the server is not abandoned.
fn should_exit_for_idle(idle_secs: u64, timeout_secs: u64, in_flight: usize) -> bool {
    timeout_secs != 0 && in_flight == 0 && idle_secs >= timeout_secs
}

pub struct DaimonosHandler {
    session: Arc<Mutex<Session>>,
    /// Request liveness shared with the idle watchdog.
    activity: Arc<ActivityTracker>,
    /// When true, re-root diagnostics are written to stderr. Mirrors the
    /// server's startup-log gate so MCP-quiet mode stays silent (Cursor
    /// surfaces subprocess stderr as `[error]`).
    startup_logs: bool,
}

impl DaimonosHandler {
    pub fn new(session: Session, activity: Arc<ActivityTracker>, startup_logs: bool) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            activity,
            startup_logs,
        }
    }

    /// Claim an in-flight slot for the duration of a request. Hold the guard
    /// for the whole handler body; dropping it early re-opens the idle window
    /// while the request is still running.
    fn begin_request(&self) -> RequestGuard {
        self.activity.begin_request()
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
            "kgl_query" => tools::get_str(args, "query"),
            "kgl_assert" => tools::get_str(args, "action"),
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

        let (saved_tokens, savings_pct) =
            analytics::compute_savings(meta.unfiltered_chars, response_chars);
        let record = ToolCallRecord {
            tool_name: name.to_string(),
            command,
            request_tokens: analytics::estimate_tokens(request_chars),
            response_tokens: analytics::estimate_tokens(response_chars),
            saved_tokens,
            savings_pct,
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

/// Dispatch a plugin or meta tool that has no opcode mapping, returning
/// `(content, is_error, meta)`. `None` means `name` does not belong to this
/// registry at all, so the caller should fall through to remote MCP dispatch.
///
/// These tools are implemented once, here, and both frontends route through this
/// entry point. Previously only the MCP adapter did, and the agent loop fell
/// through to "not available in agent mode" for all 19 of them (vikunja 1112).
pub(crate) async fn dispatch_local_tool(
    session: &mut Session,
    name: &str,
    args: &Value,
) -> Option<(String, bool, ResponseMeta)> {
    // Decide ownership of the name *before* dispatching. `dispatch_tool_inner`
    // calls `activate_tool` and records `used_tools` on entry, so probing it
    // speculatively would mutate session exposure state for remote bridge tools
    // and misspellings this registry never owned.
    if !tools::is_registry_tool(name) {
        return None;
    }

    // Drain stale meta first so a handler that sets none cannot inherit flags
    // from the previous call — the same guard `dispatch_tool` applies.
    let _ = std::mem::take(&mut session.last_response_meta);
    let result = dispatch_tool_inner(session, name, args).await;
    let meta = std::mem::take(&mut session.last_response_meta);

    match result {
        Ok(result) => Some((
            extract_result_text(&result),
            result.is_error.unwrap_or(false),
            meta,
        )),
        // The name is ours, so an error is a real failure. Returning `None` here
        // would misroute it to remote dispatch and surface as "not available".
        Err(error) => Some((format!("tool '{name}' failed: {error:?}"), true, meta)),
    }
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

    // Registry-based dispatch: route ops-backed tools through the facade.
    if let Some(resp) = tool_facade::invoke(session, name, args).await {
        session.last_response_meta = resp.meta.clone();
        return response_to_result(resp);
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

            let all = tools::tool_definitions(&session.cfg.prompts.resolved_tool_descriptions);
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
                let known: Vec<&str> = tools::mcp_tool_names();
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
            let all = tools::tool_definitions(&session.cfg.prompts.resolved_tool_descriptions);
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
                        obj.insert("verbosity".to_string(), json!(session.verbosity.as_str()));
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

        "set_verbosity" => {
            let level = match get_str(args, "level") {
                Some(s) => s,
                None => return err_text("set_verbosity requires 'level' argument".into()),
            };
            let parsed = match crate::verbosity::Verbosity::from_input(&level) {
                Some(v) => v,
                None => {
                    return err_text(format!(
                        "unknown verbosity level: {:?}. Use one of {:?}",
                        level,
                        crate::verbosity::Verbosity::valid_names()
                    ))
                }
            };
            let previous = session.verbosity;
            session.verbosity = parsed;
            ok_text(
                serde_json::to_string(&json!({
                    "verbosity": session.verbosity.as_str(),
                    "previous": previous.as_str(),
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
            info["verbosity"] = json!(session.verbosity.as_str());

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

        "pytest" => {
            let command = match get_str(args, "command") {
                Some(c) => c,
                None => return err_text("pytest requires 'command' argument".into()),
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
                .run("pytest", &command, &cwd, &env, None, extra_ref)
                .await
            {
                Ok(result) => {
                    let text = serde_json::to_string(&result.output).unwrap_or_default();
                    ok_text(text)
                }
                Err(e) => err_text(format!("pytest {command}: {e}")),
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

        "curl" => {
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
                .run("curl", "request", &cwd, &env, None, extra_ref)
                .await
            {
                Ok(result) => {
                    let text = serde_json::to_string(&result.output).unwrap_or_default();
                    ok_text(text)
                }
                Err(e) => err_text(format!("curl: {e}")),
            }
        }

        "shellcheck" => {
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
                .run("shellcheck", "check", &cwd, &env, None, extra_ref)
                .await
            {
                Ok(result) => {
                    let text = serde_json::to_string(&result.output).unwrap_or_default();
                    ok_text(text)
                }
                Err(e) => err_text(format!("shellcheck: {e}")),
            }
        }

        "npm" => {
            let command = match get_str(args, "command") {
                Some(c) => c,
                None => return err_text("npm requires 'command' argument".into()),
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
                .run("npm", &command, &cwd, &env, None, extra_ref)
                .await
            {
                Ok(result) => {
                    let text = serde_json::to_string(&result.output).unwrap_or_default();
                    ok_text(text)
                }
                Err(e) => err_text(format!("npm {command}: {e}")),
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

        // kgl_query opens the per-workspace KGL store; only needs the workspace path.
        // Wrapped in spawn_blocking: SQLite I/O and workspace walks are synchronous
        // and must not block the shared async executor.
        "kgl_query" => {
            let action = args
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let qargs = args.get("args").cloned().unwrap_or_else(|| json!({}));
            let workspace = session.workspace.clone();
            let kcfg = session.cfg.kgl.clone();
            let now = chrono::Utc::now().to_rfc3339();
            match tokio::task::spawn_blocking(move || {
                crate::kgl::query::run(&workspace, &action, &qargs, &now, &kcfg)
            })
            .await
            {
                Ok(Ok(v)) => ok_text(serde_json::to_string(&v).unwrap_or_default()),
                Ok(Err(e)) => err_text(format!("{e}")),
                Err(e) => err_text(format!("kgl_query task panicked: {e}")),
            }
        }

        // kgl_assert: agent write path for the non-derivable intent/provenance layer.
        // Wrapped in spawn_blocking: SQLite I/O must not block the async executor.
        "kgl_assert" => {
            let action = args
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let aargs = args.get("args").cloned().unwrap_or_else(|| json!({}));
            let workspace = session.workspace.clone();
            let kcfg = session.cfg.kgl.clone();
            let now = chrono::Utc::now().to_rfc3339();
            match tokio::task::spawn_blocking(move || {
                crate::kgl::assert::run(&workspace, &action, &aargs, &now, &kcfg)
            })
            .await
            {
                Ok(Ok(v)) => ok_text(serde_json::to_string(&v).unwrap_or_default()),
                Ok(Err(e)) => err_text(format!("{e}")),
                Err(e) => err_text(format!("kgl_assert task panicked: {e}")),
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

/// Decode percent-escapes (`%XX`) in a URI path component. Anything that
/// isn't a well-formed escape is passed through verbatim, so plain paths
/// (the common case) are untouched.
fn percent_decode(s: &str) -> String {
    fn hex_val(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Convert an MCP root URI into a filesystem path. The MCP spec requires
/// roots to use the `file://` scheme; anything else (http, etc.) yields
/// `None` so the caller falls back to the launch workspace.
fn root_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///abs/path` -> empty authority, `rest` begins with `/`.
    // `file://host/abs/path` -> drop the authority and keep from the
    // first `/`. A bare `file://host` with no path is not a workspace.
    let path_part = if rest.starts_with('/') {
        rest.to_string()
    } else {
        match rest.find('/') {
            Some(idx) => rest[idx..].to_string(),
            None => return None,
        }
    };
    if path_part.is_empty() {
        return None;
    }
    Some(PathBuf::from(percent_decode(&path_part)))
}

/// Pick the first client-advertised root that resolves to an existing
/// directory. Roots are tried in order so the client's preferred root
/// (listed first) wins. Returns the canonicalized path.
fn pick_workspace_root(roots: &[Root]) -> Option<PathBuf> {
    roots
        .iter()
        .filter_map(|r| root_uri_to_path(&r.uri))
        .filter_map(|p| p.canonicalize().ok())
        .find(|p| p.is_dir())
}

/// Re-root a live session onto `new_root`: repoint the workspace, working
/// directory, snapshot store, and trigram index, and drop the read cache
/// (its keys are canonical paths under the old root). The KGL store path is
/// derived from `session.workspace` at call time, so it follows automatically.
/// A fresh `WorkspaceIndex` is spawned because `WorkspaceIndex::root` is
/// immutable by design — the index owns its root for the life of the build.
fn apply_reroot(session: &mut Session, new_root: PathBuf) {
    session.workspace = new_root.clone();
    session.cwd = new_root.clone();
    session.snapshot_store = SnapshotStore::new(new_root.clone());
    session.read_cache.clear();
    let idx = Arc::new(WorkspaceIndex::new(new_root, &session.cfg.index, false));
    idx.spawn_reindex();
    session.index = Some(idx);
}

#[async_trait]
impl ServerHandler for DaimonosHandler {
    /// Honor the MCP `roots` protocol (vikunja #46). After the client
    /// finishes initialization we ask it for its workspace roots and, if it
    /// advertises one, re-root the session onto the client's actual project
    /// instead of whatever `-w`/cwd the launcher hardcoded. Clients that
    /// don't support roots keep the launch workspace — a pure superset of
    /// the old behavior.
    async fn on_initialized(&self, runtime: Arc<dyn McpServer>) {
        if runtime.client_supports_root_list() != Some(true) {
            return;
        }
        let roots = match runtime.request_root_list(None).await {
            Ok(r) => r.roots,
            Err(e) => {
                if self.startup_logs {
                    eprintln!("daimonos: roots/list request failed: {e}");
                }
                return;
            }
        };
        let new_root = match pick_workspace_root(&roots) {
            Some(p) => p,
            None => return,
        };
        let mut session = self.session.lock().await;
        let current = session
            .workspace
            .canonicalize()
            .unwrap_or_else(|_| session.workspace.clone());
        if new_root == current {
            return;
        }
        if self.startup_logs {
            eprintln!(
                "daimonos: re-rooting workspace {:?} -> client root {:?}",
                current, new_root
            );
        }
        apply_reroot(&mut session, new_root);
    }

    async fn handle_list_tools_request(
        &self,
        _request: Option<PaginatedRequestParams>,
        _runtime: Arc<dyn McpServer>,
    ) -> std::result::Result<ListToolsResult, RpcError> {
        let _activity = self.begin_request();
        let session = self.session.lock().await;
        let descriptions = &session.cfg.prompts.resolved_tool_descriptions;
        let all = tools::tool_definitions(descriptions);
        let workspace = &session.workspace;

        let full_tool_schemas = config::effective_full_tool_schemas(&session.cfg);
        let verbosity = session.verbosity;
        let visible: Vec<Tool> = all
            .into_iter()
            .filter(|t| session.exposed_tools.contains(&t.name))
            .filter(|t| tools::passes_context_check(&t.name, workspace))
            .map(|t| {
                let already_used = session.used_tools.contains(&t.name);
                tools::render_list_tool(t, descriptions, verbosity, full_tool_schemas, already_used)
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
        runtime: Arc<dyn McpServer>,
    ) -> std::result::Result<CallToolResult, CallToolError> {
        // Held for the whole call: a long or hung tool must not read as idle.
        let _activity = self.begin_request();
        let args: Value = serde_json::to_value(&params.arguments).unwrap_or(Value::Null);

        // execute_script needs Arc<Mutex<Session>> — handle it before locking.
        if params.name == "execute_script" {
            // Keep the (Send) script result across the lock/notify awaits and
            // build the non-Send CallToolResult only afterwards, so the handler
            // future stays Send.
            let script_res = run_execute_script(self.session.clone(), &args).await;
            // A script may activate on-demand tools via nested dispatch.
            let changed = self.session.lock().await.take_tools_changed();
            if changed {
                spawn_notify_tools_changed(&runtime);
            }
            return script_result_to_tool_result(script_res);
        }

        let mut session = self.session.lock().await;

        if params.name == "batch" {
            let ops = match args.get("ops").and_then(|v| v.as_array()) {
                Some(arr) => arr.clone(),
                None => return err_text("batch requires 'ops' array".into()),
            };

            // Collect successful file sub-ops to observe AFTER the batch
            // completes (KGL observe, gated off by default). Recording inside
            // the loop would do sync SQLite I/O while the session mutex is held;
            // and the early return below previously skipped the observe hook for
            // batched ops entirely.
            let observe_on = crate::kgl::observe::enabled();
            let mut observed_ops: Vec<(String, Value)> = Vec::new();

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
                            if observe_on {
                                observed_ops.push((tool.clone(), sub_args.clone()));
                            }
                            let parsed: Value = serde_json::from_str(&text).unwrap_or(json!(text));
                            results.push(json!({"ok": true, "tool": tool, "data": parsed}));
                        }
                    }
                    Err(e) => {
                        results.push(json!({"ok": false, "tool": tool, "error": format!("{e:?}")}));
                    }
                }
            }

            let payload = serde_json::to_string(&results).unwrap_or_default();
            // Read the dirty flag before the observe path may drop the lock.
            let tools_changed = session.take_tools_changed();
            if observe_on && !observed_ops.is_empty() {
                let now = chrono::Utc::now().to_rfc3339();
                let sid = session
                    .external_session_id
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());
                let ws = session.workspace.clone();
                let cwd = session.cwd.clone();
                let kcfg = session.cfg.kgl.clone();
                drop(session); // release the lock before sync SQLite I/O
                for (tool, sub_args) in &observed_ops {
                    if let Err(e) = crate::kgl::observe::record_file_op(
                        &ws, &cwd, &sid, tool, sub_args, &now, &kcfg,
                    ) {
                        eprintln!("kgl observe (batch/{tool}): {e}");
                    }
                }
            }
            if tools_changed {
                spawn_notify_tools_changed(&runtime);
            }
            return ok_text(payload);
        }

        let result = dispatch_tool(&mut session, &params.name, &args).await;
        // Read the dirty flag before the observe path may drop the lock.
        let tools_changed = session.take_tools_changed();
        // Observed-provenance capture (KGL), gated off by default. Records direct
        // file ops as observed reads/mutates edges from the session. Best-effort:
        // never affects the tool result. The session lock is released before the
        // sync SQLite write so observe can't serialize concurrent requests.
        if crate::kgl::observe::enabled() {
            if let Ok(r) = &result {
                if !r.is_error.unwrap_or(false) {
                    let now = chrono::Utc::now().to_rfc3339();
                    let sid = session
                        .external_session_id
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string());
                    let ws = session.workspace.clone();
                    let cwd = session.cwd.clone();
                    let kcfg = session.cfg.kgl.clone();
                    drop(session); // release the lock before sync SQLite I/O
                    if let Err(e) = crate::kgl::observe::record_file_op(
                        &ws,
                        &cwd,
                        &sid,
                        &params.name,
                        &args,
                        &now,
                        &kcfg,
                    ) {
                        eprintln!("kgl observe ({}): {e}", params.name);
                    }
                }
            }
        }
        if tools_changed {
            spawn_notify_tools_changed(&runtime);
        }
        result
    }
}

/// Emit `notifications/tools/list_changed` to the connected client without
/// awaiting in the caller. Spawned rather than awaited so the non-`Send`
/// `CallToolResult` a handler is about to return need not be held across an
/// await, and so a slow/failed notification never blocks or fails the tool
/// call that triggered it. The server advertises `tools.list_changed: true`,
/// so supporting clients re-fetch `tools/list` on receipt (vikunja #993).
fn spawn_notify_tools_changed(runtime: &Arc<dyn McpServer>) {
    let runtime = Arc::clone(runtime);
    tokio::spawn(async move {
        if let Err(e) = runtime.notify_tool_list_changed(None).await {
            eprintln!("daimonos: tools/list_changed notification failed: {e}");
        }
    });
}

/// Execute a Starlark tool script against shared session state. Both MCP
/// transports use this path so `execute_script` has identical argument,
/// timeout, and used-tool behavior (vikunja #998).
async fn run_execute_script(
    session: Arc<Mutex<Session>>,
    args: &Value,
) -> Result<script::ScriptResult, String> {
    session
        .lock()
        .await
        .used_tools
        .insert("execute_script".into());
    let code = args
        .get("code")
        .and_then(Value::as_str)
        .ok_or_else(|| "execute_script requires 'code' argument".to_string())?
        .to_string();
    let timeout_secs = args.get("timeout").and_then(Value::as_i64).unwrap_or(60) as u64;
    script::execute(&code, session, std::time::Duration::from_secs(timeout_secs)).await
}

/// Convert the Send-safe script outcome only after all handler awaits finish;
/// `CallToolError` itself is not Send.
fn script_result_to_tool_result(
    script_result: Result<script::ScriptResult, String>,
) -> std::result::Result<CallToolResult, CallToolError> {
    match script_result {
        Ok(result) => {
            let mut response = json!({"result": result.value});
            if !result.logs.is_empty() {
                response["logs"] = json!(result.logs);
            }
            ok_text(serde_json::to_string(&response).unwrap_or_default())
        }
        Err(error) => err_text(error),
    }
}

// --- Proactive workspace context ---

/// Build dynamic instructions that include workspace-specific context
/// so the model has useful information without a separate tool call.
/// Orientation hint nudging agents to query the KGL graph first (and to record
/// intent as they work). Only emitted when KGL auto-indexing is on, so the
/// graph actually exists. Pure (takes the gate) so it's testable without env.
async fn kgl_instructions_hint(kgl_enabled: bool, cfg: &crate::config::Config) -> Option<String> {
    if !kgl_enabled {
        return None;
    }
    Some(crate::prompts::kgl_hint(cfg).await)
}

async fn build_instructions(workspace: &std::path::Path, cfg: &crate::config::Config) -> String {
    // The static instruction sentences are an externalized prompt (vikunja
    // #974); the dynamic workspace context below is appended in code.
    let mut parts = vec![
        crate::prompts::mcp_instructions(cfg)
            .await
            .trim_end()
            .to_string(),
        format!("Workspace: {}", workspace.display()),
    ];

    parts.push(format!(
        "Starlark tool functions for execute_script:\n{}",
        script::tool_signatures()
    ));

    if let Some(hint) = kgl_instructions_hint(crate::kgl::autoindex::enabled(), cfg).await {
        parts.push(hint.trim_end().to_string());
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
    activity: Arc<ActivityTracker>,
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
            let (in_flight, idle) = activity.liveness_snapshot();
            if should_exit_for_idle(idle, timeout_secs, in_flight) {
                // Logged at WARN, not just stderr-under-a-flag: this exit can
                // discard a session's remaining work, and when it was only an
                // `eprintln!` the shutdown left no trace in the JSON log at all.
                tracing::warn!(
                    event = "idle_shutdown",
                    idle_secs = idle,
                    timeout_secs,
                    "idle watchdog exiting to release resources"
                );
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
    let activity = Arc::new(ActivityTracker::new());
    spawn_idle_watchdog(
        idle_timeout_secs,
        activity.clone(),
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
    // Apply the DAIMONOS_MCP_VERBOSITY env override here at the startup edge
    // rather than inside Session::new, so the constructor stays free of
    // process-global env reads (vikunja #181).
    session.verbosity = config::effective_verbosity(&session.cfg);

    let instructions = build_instructions(&workspace, &session.cfg).await;
    let handler = DaimonosHandler::new(session, activity, startup_logs);

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
        meta: Some(mcp_identity_meta()),
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

// ---------------------------------------------------------------------------
// MCP-over-socket: serve one connection with a fresh Session
// ---------------------------------------------------------------------------

fn socket_jsonrpc_ok(id: Option<Value>, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn socket_jsonrpc_error(id: Option<Value>, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn socket_list_tools_json(session: &Session) -> Vec<Value> {
    let descriptions = &session.cfg.prompts.resolved_tool_descriptions;
    let all = tools::tool_definitions(descriptions);
    let workspace = &session.workspace;
    let full_tool_schemas = config::effective_full_tool_schemas(&session.cfg);
    let verbosity = session.verbosity;
    all.into_iter()
        .filter(|t| session.exposed_tools.contains(&t.name))
        .filter(|t| tools::passes_context_check(&t.name, workspace))
        .map(|t| {
            let already_used = session.used_tools.contains(&t.name);
            let t = tools::render_list_tool(
                t,
                descriptions,
                verbosity,
                full_tool_schemas,
                already_used,
            );
            serde_json::to_value(&t).unwrap_or(Value::Null)
        })
        .collect()
}

fn socket_call_tool_to_json(result: std::result::Result<CallToolResult, CallToolError>) -> Value {
    match result {
        Ok(r) => serde_json::to_value(&r).unwrap_or(json!({"content": [], "isError": false})),
        Err(e) => json!({
            "content": [{"type": "text", "text": format!("{e}")}],
            "isError": true,
        }),
    }
}

fn socket_client_supports_roots(initialize_request: &Value) -> bool {
    initialize_request
        .pointer("/params/capabilities/roots")
        .is_some()
}

/// Apply a successful socket `roots/list` response to the live session.
/// Invalid/missing/non-directory roots preserve the launch workspace.
fn apply_socket_roots_response(session: &mut Session, response: &Value) -> bool {
    let roots = match response
        .pointer("/result/roots")
        .cloned()
        .and_then(|value| serde_json::from_value::<Vec<Root>>(value).ok())
    {
        Some(roots) => roots,
        None => return false,
    };
    let new_root = match pick_workspace_root(&roots) {
        Some(root) => root,
        None => return false,
    };
    let current = session
        .workspace
        .canonicalize()
        .unwrap_or_else(|_| session.workspace.clone());
    if new_root == current {
        return false;
    }
    apply_reroot(session, new_root);
    true
}

/// Serve one MCP session over an already-connected `UnixStream`.
///
/// Each connection owns a freshly-constructed `Session` so sessions are
/// fully isolated: cwd, read-cache, used-tools, and analytics are not
/// shared across concurrent connections.
pub async fn serve_one_mcp(stream: tokio::net::UnixStream, session: Session) -> anyhow::Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();
    let instructions = build_instructions(&session.workspace, &session.cfg).await;
    let session = Arc::new(Mutex::new(session));
    let mut client_supports_roots = false;
    let mut pending_roots_request: Option<Value> = None;

    while let Ok(Some(line)) = lines.next_line().await {
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str);

        // Response to the server-initiated roots/list request.
        if method.is_none()
            && pending_roots_request.is_some()
            && id.as_ref() == pending_roots_request.as_ref()
        {
            let mut session = session.lock().await;
            apply_socket_roots_response(&mut session, &req);
            pending_roots_request = None;
            continue;
        }
        // Ignore other response-shaped messages rather than replying with a
        // method-not-found error to a JSON-RPC response.
        let Some(method) = method else {
            continue;
        };

        // Set when a tools/call grows the exposed set, so we can emit a
        // tools/list_changed notification after the response (vikunja #993).
        let mut tools_changed = false;
        let mut server_request: Option<Value> = None;

        let response_opt: Option<Value> = match method {
            "initialize" => {
                client_supports_roots = socket_client_supports_roots(&req);
                Some(socket_jsonrpc_ok(
                    id,
                    json!({
                        "protocolVersion": "2025-11-25",
                        "capabilities": {"tools": {"listChanged": true}},
                        "serverInfo": {
                            "name": "daimonos",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                        "instructions": instructions,
                        "_meta": mcp_identity_meta(),
                    }),
                ))
            }
            "notifications/initialized" => {
                if client_supports_roots && pending_roots_request.is_none() {
                    let request_id = json!("daimonos-roots-1");
                    pending_roots_request = Some(request_id.clone());
                    server_request = Some(json!({
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "method": "roots/list",
                    }));
                }
                None
            }
            "tools/list" => {
                let session = session.lock().await;
                let tools_json = socket_list_tools_json(&session);
                Some(socket_jsonrpc_ok(id, json!({"tools": tools_json})))
            }
            "tools/call" => {
                let params = req.get("params").cloned().unwrap_or(Value::Null);
                let name = params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = params.get("arguments").cloned().unwrap_or(Value::Null);
                let result = if name == "execute_script" {
                    // Keep the Send-safe script outcome across the lock await;
                    // construct CallToolResult only afterwards.
                    let script_result = run_execute_script(Arc::clone(&session), &args).await;
                    tools_changed = session.lock().await.take_tools_changed();
                    script_result_to_tool_result(script_result)
                } else {
                    let mut session = session.lock().await;
                    let result = dispatch_tool(&mut session, &name, &args).await;
                    tools_changed = session.take_tools_changed();
                    result
                };
                Some(socket_jsonrpc_ok(id, socket_call_tool_to_json(result)))
            }
            "ping" => Some(socket_jsonrpc_ok(id, json!({}))),
            _ => Some(socket_jsonrpc_error(
                id,
                -32601,
                &format!("method not found: {method}"),
            )),
        };

        if let Some(resp) = response_opt {
            let mut out = serde_json::to_string(&resp)?;
            out.push('\n');
            writer.write_all(out.as_bytes()).await?;
            writer.flush().await?;
        }

        if let Some(request) = server_request {
            let mut out = serde_json::to_string(&request)?;
            out.push('\n');
            writer.write_all(out.as_bytes()).await?;
            writer.flush().await?;
        }

        // Emit the tool-list-changed notification after the triggering
        // response so clients that support it re-fetch tools/list.
        if tools_changed {
            let note = json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"});
            let mut out = serde_json::to_string(&note)?;
            out.push('\n');
            writer.write_all(out.as_bytes()).await?;
            writer.flush().await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `dispatch_tool_inner` calls `activate_tool` and records `used_tools` on
    /// entry, so `dispatch_local_tool` has to establish that a name belongs to
    /// this registry *before* dispatching. The agent loop calls it for every
    /// non-opcode name on the way to remote dispatch, so probing speculatively
    /// would leave exposure and usage state polluted with remote bridge tools and
    /// typos that were never local (vikunja 1112, codeJung review of PR #110).
    #[tokio::test]
    async fn dispatch_local_tool_declines_foreign_names_without_touching_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::new(
            dir.path().to_path_buf(),
            std::sync::Arc::new(crate::config::Config::default()),
        );
        let exposed_before = session.exposed_tools.clone();
        let used_before = session.used_tools.clone();

        // A remote MCP bridge name, and a misspelling of a real local tool.
        for name in ["mcp__other_server__do_thing", "shelcheck"] {
            assert!(
                dispatch_local_tool(&mut session, name, &json!({}))
                    .await
                    .is_none(),
                "{name} is not in this registry and must be declined"
            );
            assert!(
                !session.used_tools.contains(name),
                "{name} leaked into used_tools"
            );
            assert!(
                !session.exposed_tools.contains(name),
                "{name} leaked into exposed_tools"
            );
        }

        assert_eq!(session.exposed_tools, exposed_before);
        assert_eq!(session.used_tools, used_before);
    }

    // --- Idle watchdog: an in-flight request is not idleness (vikunja #1078) ---

    #[test]
    fn idle_exit_fires_once_the_window_passes_with_nothing_in_flight() {
        assert!(should_exit_for_idle(600, 600, 0));
        assert!(should_exit_for_idle(601, 600, 0));
    }

    #[test]
    fn idle_exit_is_vetoed_while_a_request_is_in_flight() {
        // The regression. A single long or hung tool call sends no new
        // requests, so the clock goes stale while the server is genuinely
        // busy. Killing it there loses the call's result and surfaces to the
        // client as an opaque transport error.
        assert!(!should_exit_for_idle(600, 600, 1));
        assert!(!should_exit_for_idle(86_400, 600, 1));
        assert!(!should_exit_for_idle(86_400, 600, 4));
    }

    #[test]
    fn idle_exit_does_not_fire_before_the_window() {
        assert!(!should_exit_for_idle(599, 600, 0));
        assert!(!should_exit_for_idle(0, 600, 0));
    }

    #[test]
    fn idle_exit_is_disabled_by_a_zero_timeout() {
        assert!(!should_exit_for_idle(u64::MAX, 0, 0));
    }

    #[test]
    fn request_guard_tracks_and_releases_in_flight_slots() {
        let activity = Arc::new(ActivityTracker::new());
        assert_eq!(activity.in_flight(), 0);

        let first = activity.begin_request();
        assert_eq!(activity.in_flight(), 1);

        let second = activity.begin_request();
        assert_eq!(activity.in_flight(), 2);

        drop(second);
        assert_eq!(activity.in_flight(), 1);

        drop(first);
        assert_eq!(activity.in_flight(), 0);
    }

    #[test]
    fn request_guard_releases_its_slot_on_unwind() {
        // A handler that panics must not leave the slot claimed forever, which
        // would disable the watchdog for the life of the process.
        let activity = Arc::new(ActivityTracker::new());
        let panicking = Arc::clone(&activity);

        let result = std::panic::catch_unwind(move || {
            let _guard = panicking.begin_request();
            assert_eq!(panicking.in_flight(), 1);
            panic!("handler blew up");
        });

        assert!(result.is_err());
        assert_eq!(activity.in_flight(), 0);
    }

    #[test]
    fn releasing_the_last_slot_leaves_a_fresh_idle_window() {
        // A long call ends with a clock that is stale by its whole duration.
        // Releasing the slot must refresh the clock, or the watchdog exits the
        // instant the veto lifts — the same kill, moved to completion time.
        let activity = Arc::new(ActivityTracker::new());
        let guard = activity.begin_request();

        // Simulate a call that ran far longer than any timeout.
        activity
            .last_activity
            .store(now_unix_secs().saturating_sub(10_000), Ordering::Relaxed);
        assert!(
            !should_exit_for_idle(activity.idle_secs(), 600, activity.in_flight()),
            "in-flight work must veto the exit even with a very stale clock"
        );

        drop(guard);

        assert_eq!(activity.in_flight(), 0);
        assert!(
            !should_exit_for_idle(activity.idle_secs(), 600, activity.in_flight()),
            "releasing the last in-flight slot must restart the idle window \
             from completion time, not leave the pre-call timestamp in place"
        );
    }

    #[test]
    fn liveness_snapshot_never_reports_a_stale_clock_as_unoccupied() {
        // The sampling order is what makes the pair usable. With the clock read
        // first, a call completing between the two loads yields (stale idle,
        // in_flight 0) and the watchdog exits on a server that just finished
        // work. The snapshot must never hand back that combination.
        let activity = Arc::new(ActivityTracker::new());
        let guard = activity.begin_request();
        activity
            .last_activity
            .store(now_unix_secs().saturating_sub(10_000), Ordering::Relaxed);

        let (in_flight, idle) = activity.liveness_snapshot();
        assert_eq!(in_flight, 1);
        assert!(idle >= 10_000);
        assert!(!should_exit_for_idle(idle, 600, in_flight));

        drop(guard);

        let (in_flight, idle) = activity.liveness_snapshot();
        assert_eq!(in_flight, 0);
        assert!(
            idle <= 1,
            "observing an unoccupied tracker must come with the refreshed \
             timestamp, got idle={idle}s"
        );
        assert!(!should_exit_for_idle(idle, 600, in_flight));
    }

    #[test]
    fn a_fresh_tracker_starts_idle_and_unoccupied() {
        let activity = ActivityTracker::new();
        assert_eq!(activity.in_flight(), 0);
        // Freshly stamped, so within the same second.
        assert!(activity.idle_secs() <= 1);
    }

    #[test]
    fn response_to_result_ok() {
        let resp = Response::ok(json!({"lines": 10}));
        let result = response_to_result(resp).unwrap();
        assert!(result.is_error.is_none() || !result.is_error.unwrap());
    }

    // --- MCP roots support (vikunja #46): honor the client's workspace root ---

    fn root(uri: &str) -> Root {
        Root {
            meta: None,
            name: None,
            uri: uri.into(),
        }
    }

    #[test]
    fn root_uri_to_path_parses_standard_file_uri() {
        assert_eq!(
            root_uri_to_path("file:///home/user/proj"),
            Some(PathBuf::from("/home/user/proj"))
        );
    }

    #[test]
    fn root_uri_to_path_handles_authority_host() {
        // `file://host/abs` -> drop the authority, keep the absolute path.
        assert_eq!(
            root_uri_to_path("file://localhost/srv/work"),
            Some(PathBuf::from("/srv/work"))
        );
    }

    #[test]
    fn root_uri_to_path_percent_decodes() {
        assert_eq!(
            root_uri_to_path("file:///home/user/my%20proj"),
            Some(PathBuf::from("/home/user/my proj"))
        );
    }

    #[test]
    fn root_uri_to_path_rejects_non_file_schemes() {
        assert_eq!(root_uri_to_path("https://example.com/x"), None);
        assert_eq!(root_uri_to_path("file://host"), None);
        assert_eq!(root_uri_to_path("not-a-uri"), None);
    }

    #[test]
    fn pick_workspace_root_picks_first_existing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let canon = dir.path().canonicalize().unwrap();
        let uri = format!("file://{}", canon.display());
        // First root is bogus (non-existent), second is the real dir, third
        // is a non-file scheme — picker must skip to the real one.
        let roots = vec![
            root("file:///definitely/not/here/zzz"),
            root(&uri),
            root("https://example.com"),
        ];
        assert_eq!(pick_workspace_root(&roots), Some(canon));
    }

    #[test]
    fn pick_workspace_root_none_when_no_valid_root() {
        let roots = vec![root("https://example.com"), root("file:///no/such/dir/q")];
        assert_eq!(pick_workspace_root(&roots), None);
    }

    #[tokio::test]
    async fn apply_reroot_repoints_workspace_cwd_and_index() {
        use crate::config::Config;

        let old = tempfile::tempdir().unwrap();
        let new = tempfile::tempdir().unwrap();
        let new_canon = new.path().canonicalize().unwrap();
        // A uniquely-named file in the new root so we can prove the index
        // rebuilt against it (and not the old root).
        std::fs::write(
            new.path().join("rerooted_marker.rs"),
            "fn rerooted_sentinel() {}\n",
        )
        .unwrap();

        let mut session = Session::new(old.path().to_path_buf(), Arc::new(Config::default()));
        // Seed the read cache so we can assert it is cleared on re-root.
        session.read_cache.insert(
            old.path().join("stale.txt"),
            crate::session::ReadCacheEntry { hash: 1, lines: 1 },
        );

        apply_reroot(&mut session, new_canon.clone());

        assert_eq!(session.workspace, new_canon);
        assert_eq!(session.cwd, new_canon);
        assert!(session.read_cache.is_empty());
        assert!(session.index.is_some());

        // The freshly spawned index runs on a blocking task; poll until it
        // has indexed the marker file in the new root.
        let idx = session.index.clone().unwrap();
        let mut found = false;
        for _ in 0..50 {
            if !idx.search("rerooted_sentinel", 10).await.is_empty() {
                found = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        }
        assert!(found, "re-rooted index did not pick up new-root file");
    }

    #[test]
    fn socket_roots_capability_detection_requires_roots_key() {
        assert!(socket_client_supports_roots(&json!({
            "params": {"capabilities": {"roots": {"listChanged": true}}}
        })));
        assert!(!socket_client_supports_roots(&json!({
            "params": {"capabilities": {}}
        })));
        assert!(!socket_client_supports_roots(&json!({})));
    }

    #[tokio::test]
    async fn socket_roots_response_reroots_only_for_valid_result() {
        use crate::config::Config;

        let launch = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let project_canon = project.path().canonicalize().unwrap();
        let mut session = Session::new(launch.path().to_path_buf(), Arc::new(Config::default()));

        assert!(!apply_socket_roots_response(
            &mut session,
            &json!({"error": {"code": -1, "message": "no roots"}}),
        ));
        assert_eq!(session.workspace, launch.path());

        assert!(apply_socket_roots_response(
            &mut session,
            &json!({
                "result": {
                    "roots": [{
                        "uri": format!("file://{}", project_canon.display()),
                        "name": "project"
                    }]
                }
            }),
        ));
        assert_eq!(session.workspace, project_canon);
        assert_eq!(session.cwd, project_canon);
    }

    #[tokio::test]
    async fn shared_execute_script_runner_writes_through_session() {
        use crate::config::Config;

        let workspace = tempfile::tempdir().unwrap();
        let session = Arc::new(Mutex::new(Session::new(
            workspace.path().to_path_buf(),
            Arc::new(Config::default()),
        )));
        let outcome = run_execute_script(
            Arc::clone(&session),
            &json!({
                "code": "write_file('socket-script.txt', 'ok')\nresult = 'done'"
            }),
        )
        .await
        .expect("script should execute");

        assert_eq!(outcome.value, json!("done"));
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("socket-script.txt")).unwrap(),
            "ok"
        );
        assert!(session.lock().await.used_tools.contains("execute_script"));
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
        let descriptions = crate::tool_descriptions::ToolDescriptions::default();
        let defs = tools::tool_definitions(&descriptions);
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
        let descriptions = crate::tool_descriptions::ToolDescriptions::default();
        for tool in tools::tool_definitions(&descriptions) {
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
        let descriptions = crate::tool_descriptions::ToolDescriptions::default();
        let defs = tools::tool_definitions(&descriptions);
        let names: Vec<&str> = defs.iter().map(|t| t.name.as_str()).collect();
        let unique: std::collections::HashSet<&&str> = names.iter().collect();
        assert_eq!(names.len(), unique.len(), "duplicate tool names found");
    }

    #[test]
    fn schema_token_savings_benchmark() {
        let descriptions = crate::tool_descriptions::ToolDescriptions::default();
        let all = tools::tool_definitions(&descriptions);

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

    #[tokio::test]
    async fn kgl_hint_is_gated_and_mentions_orient() {
        assert!(
            kgl_instructions_hint(false, &crate::config::Config::default())
                .await
                .is_none()
        );
        let hint = kgl_instructions_hint(true, &crate::config::Config::default())
            .await
            .expect("hint when enabled");
        assert!(hint.contains("orient"));
        assert!(hint.contains("kgl_assert"));
    }

    #[tokio::test]
    async fn build_instructions_includes_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let instructions = build_instructions(dir.path(), &crate::config::Config::default()).await;
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
        let instructions = build_instructions(dir.path(), &crate::config::Config::default()).await;
        assert!(instructions.contains("Rust (Cargo)"));
    }

    #[tokio::test]
    async fn build_instructions_detects_git() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir(dir.path().join(".git"))
            .await
            .unwrap();
        let instructions = build_instructions(dir.path(), &crate::config::Config::default()).await;
        assert!(instructions.contains("VCS: git"));
    }

    #[tokio::test]
    async fn build_instructions_lists_dirs() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir(dir.path().join("src")).await.unwrap();
        tokio::fs::create_dir(dir.path().join("tests"))
            .await
            .unwrap();
        let instructions = build_instructions(dir.path(), &crate::config::Config::default()).await;
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

    // --- verbosity dial (vikunja #181) ---

    /// `set_verbosity` must mutate the session level and surface the previous
    /// value, mirroring `set_external_session_id`'s contract.
    #[tokio::test]
    async fn set_verbosity_updates_and_reports_previous() {
        use crate::config::Config;
        use crate::verbosity::Verbosity;

        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::new(dir.path().to_path_buf(), Arc::new(Config::default()));
        // Pin a known starting point independent of any ambient env override.
        session.verbosity = Verbosity::Full;

        let result = dispatch_tool_inner(&mut session, "set_verbosity", &json!({"level": "terse"}))
            .await
            .unwrap();
        let payload: Value = serde_json::from_str(&extract_result_text(&result)).unwrap();
        assert_eq!(payload["verbosity"], json!("terse"));
        assert_eq!(payload["previous"], json!("full"));
        assert_eq!(session.verbosity, Verbosity::Terse);

        let result =
            dispatch_tool_inner(&mut session, "set_verbosity", &json!({"level": "compact"}))
                .await
                .unwrap();
        let payload: Value = serde_json::from_str(&extract_result_text(&result)).unwrap();
        assert_eq!(payload["verbosity"], json!("compact"));
        assert_eq!(payload["previous"], json!("terse"));
        assert_eq!(session.verbosity, Verbosity::Compact);
    }

    /// An unrecognized level is rejected and leaves the session unchanged.
    #[tokio::test]
    async fn set_verbosity_rejects_unknown_level() {
        use crate::config::Config;
        use crate::verbosity::Verbosity;

        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::new(dir.path().to_path_buf(), Arc::new(Config::default()));
        session.verbosity = Verbosity::Full;

        let result = dispatch_tool_inner(&mut session, "set_verbosity", &json!({"level": "loud"}))
            .await
            .unwrap();
        assert!(extract_result_text(&result).contains("unknown verbosity level"));
        assert_eq!(session.verbosity, Verbosity::Full);
    }

    /// The `session_stats` session scope must echo the active verbosity level.
    #[tokio::test]
    async fn session_stats_session_scope_includes_verbosity() {
        use crate::config::Config;
        use crate::verbosity::Verbosity;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let mut session = Session::new(dir.path().to_path_buf(), Arc::new(Config::default()));
        session.verbosity = Verbosity::Compact;
        let analytics_dir = TempDir::new().unwrap();
        session.analytics = Some(Arc::new(
            AnalyticsStore::new(&analytics_dir.path().join("a.db"), 90).unwrap(),
        ));

        let result =
            dispatch_tool_inner(&mut session, "session_stats", &json!({"scope": "session"}))
                .await
                .unwrap();
        let payload: Value = serde_json::from_str(&extract_result_text(&result)).unwrap();
        assert_eq!(payload["verbosity"], json!("compact"));
    }

    /// `workspace_info` must surface the active verbosity level.
    #[tokio::test]
    async fn workspace_info_includes_verbosity() {
        use crate::config::Config;
        use crate::verbosity::Verbosity;

        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::new(dir.path().to_path_buf(), Arc::new(Config::default()));
        session.verbosity = Verbosity::Terse;

        let result = dispatch_tool_inner(&mut session, "workspace_info", &json!({}))
            .await
            .unwrap();
        let payload: Value = serde_json::from_str(&extract_result_text(&result)).unwrap();
        assert_eq!(payload["verbosity"], json!("terse"));
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

    // --- saved_tokens wiring ---

    async fn session_with_analytics(
        dir: &std::path::Path,
    ) -> (Session, Arc<crate::analytics::AnalyticsStore>) {
        let db = dir.join("analytics.db");
        let store = Arc::new(crate::analytics::AnalyticsStore::new(&db, 90).unwrap());
        let mut cfg = crate::config::Config::default();
        cfg.analytics.enabled = true;
        let mut session = Session::new(dir.to_path_buf(), Arc::new(cfg));
        session.analytics = Some(store.clone());
        (session, store)
    }

    #[tokio::test]
    async fn read_dedup_records_positive_saved_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let content = "hello world this is a file with some content\n".repeat(20);
        std::fs::write(dir.path().join("dup.txt"), &content).unwrap();
        let (mut session, store) = session_with_analytics(dir.path()).await;

        let args = json!({"path": "dup.txt"});
        // First read — cache miss, nothing saved
        let _ = dispatch_tool(&mut session, "read_file", &args)
            .await
            .unwrap();
        // Second read — dedup hit; saved_tokens should reflect suppressed content
        let _ = dispatch_tool(&mut session, "read_file", &args)
            .await
            .unwrap();

        // Give the async record write a moment to land
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stats = store.session_summary();
        assert!(
            stats.total_saved_tokens > 0,
            "read_dedup must record saved_tokens > 0; got {}",
            stats.total_saved_tokens
        );
    }

    #[tokio::test]
    async fn filtered_exec_records_positive_saved_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let (mut session, store) = session_with_analytics(dir.path()).await;

        // `make --version` is recognized by exec_filter and returns compact JSON
        let args = json!({"command": "make", "args": ["--version"]});
        let result = dispatch_tool(&mut session, "exec", &args).await.unwrap();
        let payload: Value = serde_json::from_str(&extract_result_text(&result)).unwrap();
        // Only assert saved_tokens if filtering actually fired
        if payload.get("out").map(|v| v.is_string()).unwrap_or(false)
            && payload["out"].as_str().unwrap_or("").len() < 200
        {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let stats = store.session_summary();
            assert!(
                stats.total_saved_tokens > 0,
                "filter_applied exec must record saved_tokens > 0; got {}",
                stats.total_saved_tokens
            );
        }
    }
}
