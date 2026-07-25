#![allow(dead_code)]

use std::path::PathBuf;

use serde_json::Value;
use tracing::Instrument;

use crate::compaction::{self, CompactionEvent, CompactionPolicy, CompactionStrategy};
use crate::mcp_bridge::REMOTE_TOOL_PREFIX;
use crate::observability::ToolOutcome;
use crate::protocol::Response;
use crate::providers::{
    CompleteOpts, ContentBlock, Context, Cost, LlmProvider, Message, Role, StopReason, StreamEvent,
    ThinkingLevel, ToolSchema, Usage,
};
use crate::session::Session;
use crate::tool_facade;
use crate::tools::LIST_ALL_TOOLS_TOOL;

// --- Hook types ---

pub struct ToolCallInfo {
    pub id: String,
    pub name: String,
    pub input: Value,
}

pub enum BeforeHookResult {
    Allow,
    Block(String),
}

pub enum AfterHookResult {
    Continue,
    Terminate,
}

/// Async so approval can await a real round-trip (e.g. the ACP engine's
/// `session/request_permission`), not just a local blocking read. The
/// future borrows `info` and is always immediately `.await`ed at the call
/// site, so it doesn't need `'static`.
pub type BeforeHook = Box<
    dyn Fn(
            &ToolCallInfo,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = BeforeHookResult> + Send + '_>>
        + Send
        + Sync,
>;
pub type AfterHook = Box<dyn Fn(&ToolCallInfo, &str, bool) -> AfterHookResult + Send + Sync>;
pub type ToolProgressHook = Box<dyn Fn(&ToolCallInfo, crate::ops::ExecProgress) + Send + Sync>;

pub const UPDATE_PLAN_TOOL: &str = "update_plan";

/// The Starlark scripting tool. Dispatched specially in [`run`] because its
/// sandbox thread shares the live session via `Arc<Mutex<Session>>`.
pub const EXECUTE_SCRIPT_TOOL: &str = "execute_script";

/// Upper safety ceiling for a model-supplied `execute_script` timeout (1 hour).
/// Not a user-tunable knob — it only prevents a malformed/oversized value from
/// tying up a bounded script-thread slot near-indefinitely; it sits well above
/// any realistic script run (default is 60s).
const MAX_SCRIPT_TIMEOUT_SECS: i64 = 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanPriority {
    High,
    Medium,
    Low,
}

impl PlanPriority {
    pub const VALUES: &'static [&'static str] = &["high", "medium", "low"];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

impl PlanStatus {
    pub const VALUES: &'static [&'static str] = &["pending", "in_progress", "completed"];
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlanEntry {
    pub content: String,
    pub priority: PlanPriority,
    pub status: PlanStatus,
}

#[derive(serde::Deserialize)]
struct PlanInput {
    entries: Vec<PlanEntry>,
}

/// Parse and normalize a complete replacement plan from model tool input.
/// Empty plans are valid (clear the current plan); entry content must be
/// non-empty after trimming.
pub fn parse_plan_entries(input: &Value) -> Result<Vec<PlanEntry>, String> {
    let mut plan: PlanInput = serde_json::from_value(input.clone())
        .map_err(|error| format!("invalid update_plan input: {error}"))?;
    for (index, entry) in plan.entries.iter_mut().enumerate() {
        let content = entry.content.trim();
        if content.is_empty() {
            return Err(format!(
                "invalid update_plan input: entries[{index}].content must not be empty"
            ));
        }
        entry.content = content.to_string();
    }
    Ok(plan.entries)
}

/// Receives each complete normalized plan replacement.
pub type PlanHook = Box<dyn Fn(&[PlanEntry]) + Send + Sync>;

/// Provider-neutral outcome of a tool served outside the opcode facade (e.g. a
/// remote MCP server bridged into an ACP session — ADR-003).
pub struct RemoteToolResult {
    pub content: String,
    pub is_error: bool,
}

/// Dispatches a tool the opcode facade doesn't know. Consulted only when
/// `tool_facade::invoke` returns `None`, before the "not available" fallback.
/// `Some` = the tool was handled remotely (result used as-is); `None` = not a
/// remote tool, fall through. Async so the call can await a network round-trip.
pub type RemoteToolHook = Box<
    dyn Fn(
            &str,
            &serde_json::Value,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<RemoteToolResult>> + Send>>
        + Send
        + Sync,
>;
/// Invoked with each `StreamEvent` as a turn streams in (vikunja #957).
pub type StreamHook = Box<dyn Fn(StreamEvent) + Send + Sync>;
/// Invoked after each compaction (ADR-002) so frontends can surface an
/// informational notice (REPL line, ACP thought chunk).
pub type CompactionHook = Box<dyn Fn(&CompactionEvent) + Send + Sync>;

/// `--debug-tokens` config: where to append one JSON line per LLM API call,
/// and which subcommand (`agent`/`chat`/...) is logging it.
pub struct TokenLogConfig {
    pub path: PathBuf,
    pub label: String,
}

// --- Config and Result ---

#[derive(Default)]
pub struct AgentConfig {
    pub system: Option<String>,
    pub tools: Vec<ToolSchema>,
    pub opts: CompleteOpts,
    pub before_tool_call: Option<BeforeHook>,
    pub after_tool_call: Option<AfterHook>,
    pub on_tool_progress: Option<ToolProgressHook>,
    pub on_plan_update: Option<PlanHook>,
    /// Fallback dispatch for tools the opcode facade doesn't serve (remote MCP
    /// tools bridged into an ACP session — ADR-003). `None` = ACP bridge off.
    pub remote_tool_dispatch: Option<RemoteToolHook>,
    pub on_stream_event: Option<StreamHook>,
    pub token_log: Option<TokenLogConfig>,
    /// Context/window compaction (ADR-002). `None` = off.
    pub compaction: Option<CompactionPolicy>,
    pub on_compaction: Option<CompactionHook>,
    /// Provider handle for in-script LLM sub-calls (ADR-008 `llm_query` /
    /// `llm_query_batched`). Injected by `AgentSession::new`; `None` for
    /// one-shot/agent-cmd runs and tests, where sub-calls are unavailable.
    pub subcall_provider: Option<std::sync::Arc<dyn LlmProvider>>,
    pub generation_ordinal: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

pub struct AgentResult {
    pub messages: Vec<Message>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    pub error_message: Option<String>,
    /// Usage of the loop's FINAL API call — `usage` accumulates every call
    /// in the turn, so only this one's `prompt_tokens()` reflects the
    /// actual window occupancy the compaction trigger needs (ADR-002).
    pub last_call_usage: Usage,
    /// The final call failed as a classified context-window overflow; the
    /// reactive compaction path keys off this to compact and retry once.
    pub context_overflow: bool,
}

// --- Pure helpers ---

pub fn accumulate_usage(acc: Usage, turn: Usage) -> Usage {
    Usage {
        input: acc.input + turn.input,
        output: acc.output + turn.output,
        cache_read: acc.cache_read + turn.cache_read,
        cache_write: acc.cache_write + turn.cache_write,
        cost: Cost {
            input_usd: acc.cost.input_usd + turn.cost.input_usd,
            output_usd: acc.cost.output_usd + turn.cost.output_usd,
            cache_read_usd: acc.cost.cache_read_usd + turn.cost.cache_read_usd,
            cache_write_usd: acc.cost.cache_write_usd + turn.cost.cache_write_usd,
            total_usd: acc.cost.total_usd + turn.cost.total_usd,
        },
    }
}

fn response_to_content(resp: Response) -> String {
    if let Some(d) = resp.d {
        serde_json::to_string(&d).unwrap_or_else(|_| "{}".to_string())
    } else if let Some(m) = resp.m {
        m
    } else {
        "{}".to_string()
    }
}

/// `daimonos.tool.kind` for a tool name that never reached the opcode facade:
/// `remote` for namespaced MCP tools (ADR-003), else the static classification.
fn dispatch_tool_kind(name: &str) -> &'static str {
    crate::observability::remote_server_alias(name)
        .map(|_| "remote")
        .unwrap_or_else(|| crate::observability::tool_kind(name))
}

/// Bounded `error.type` class for a retried attempt's outcome (agent.retry
/// span). No message/content — only a normalized class.
fn retry_error_type(result: &AgentResult) -> Option<&'static str> {
    match result.stop_reason {
        StopReason::Error => Some("provider_error"),
        StopReason::Refusal => Some("refusal"),
        _ => None,
    }
}

fn append_remote_tools_to_catalog(content: String, tools: &[ToolSchema]) -> String {
    let Ok(Value::Array(mut entries)) = serde_json::from_str(&content) else {
        eprintln!("agent: list_all_tools returned a non-array catalog; remote tools omitted");
        return content;
    };
    let mut names: std::collections::HashSet<String> = entries
        .iter()
        .filter_map(|entry| entry.get("name")?.as_str().map(str::to_string))
        .collect();
    for tool in tools
        .iter()
        .filter(|tool| tool.name.starts_with(REMOTE_TOOL_PREFIX))
    {
        if names.insert(tool.name.clone()) {
            entries.push(serde_json::json!({
                "name": tool.name,
                "description": tool.description,
            }));
        }
    }
    serde_json::to_string(&entries).unwrap_or(content)
}

/// Render one `--debug-tokens` log line for a single LLM API call.
fn token_log_line(label: &str, model: &str, usage: &Usage) -> String {
    serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "cmd": label,
        "model": model,
        "input": usage.input,
        "output": usage.output,
        "cache_read": usage.cache_read,
        "cache_write": usage.cache_write,
        // Fixed-decimal string, not a bare f64: serde_json renders small floats
        // in scientific notation (e.g. 3e-6), which is diff-unfriendly and awkward
        // to parse. Six decimals = microdollar precision, enough for per-call cost.
        "cost_usd": format!("{:.6}", usage.cost.total_usd),
    })
    .to_string()
}

/// Best-effort append of one token-usage line. Never panics or propagates
/// I/O errors — a debug log must not be able to break the agent loop.
fn log_token_usage(cfg: &TokenLogConfig, model: &str, usage: &Usage) {
    use std::io::Write;
    let line = token_log_line(&cfg.label, model, usage);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&cfg.path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Render one structured compaction event line for the `--debug-tokens`
/// log — the data source for the ADR-002 strategy A/B benchmark.
fn compaction_log_line(label: &str, event: &CompactionEvent) -> String {
    serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "cmd": label,
        "event": "compaction",
        "strategy": event.strategy.as_str(),
        "evicted_turns": event.evicted_turns,
        "evicted_messages": event.evicted_messages,
        "est_tokens_before": event.est_tokens_before,
        "est_tokens_after": event.est_tokens_after,
        "summary_model": event.summary_model,
        "fallback_drop": event.fallback_drop,
    })
    .to_string()
}

/// Best-effort append of one compaction event line (same channel and
/// guarantees as [`log_token_usage`]).
fn log_compaction_event(cfg: &TokenLogConfig, event: &CompactionEvent) {
    use std::io::Write;
    let line = compaction_log_line(&cfg.label, event);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&cfg.path)
    {
        let _ = writeln!(f, "{line}");
    }
}

// --- Main loop ---

/// Check unread agent mail at a safe generation boundary. Returns a
/// metadata-only system notice and advances the model watermark exactly once.
/// Fail-open: any store/path error yields None and never fails the turn.
async fn coordination_model_notice(
    session: &std::sync::Arc<tokio::sync::Mutex<Session>>,
) -> Option<String> {
    // Snapshot only immutable routing state under the mutex; never hold it
    // across synchronous SQLite I/O.
    let (agent, watermark, db_path, busy_timeout) = {
        let s = session.lock().await;
        let cfg = &s.cfg.coordination;
        if !cfg.enabled || !cfg.notifications.enabled || !cfg.notifications.model_notice {
            return None;
        }
        (
            s.coordination_agent_name.clone()?,
            s.coordination_model_watermark,
            crate::coordination::workspace_db_path(&cfg.resolved_db_dir(), &s.workspace),
            cfg.effective_busy_timeout_ms(),
        )
    };
    let agent_for_query = agent.clone();
    let summary = match tokio::task::spawn_blocking(move || {
        crate::coordination::CoordinationStore::open_with(&db_path, busy_timeout)
            .and_then(|store| store.unread_summary(&agent_for_query, watermark))
    })
    .await
    {
        Ok(Ok(summary)) => summary?,
        Ok(Err(error)) => {
            tracing::warn!(target: "daimonos::coordination", event="notification_check_failed", error=%error);
            return None;
        }
        Err(error) => {
            tracing::warn!(target: "daimonos::coordination", event="notification_task_failed", error=%error);
            return None;
        }
    };
    // Advance only if this is still the same binding/watermark snapshot.
    let mut s = session.lock().await;
    if s.coordination_agent_name.as_deref() != Some(agent.as_str())
        || s.coordination_model_watermark != watermark
    {
        return None;
    }
    s.coordination_model_watermark = summary.newest_message_id;
    Some(format!(
        "[DAIMONOS COORDINATION NOTICE]\nYou have {} new unread agent-mail message(s) for {}. Highest importance: {}. Call fetch_inbox(agent=\"{}\", unread_only=true) before continuing if the messages may affect your current work.",
        summary.count, agent, summary.highest_importance, agent
    ))
}

pub async fn run(
    provider: &dyn LlmProvider,
    session: std::sync::Arc<tokio::sync::Mutex<Session>>,
    initial_messages: Vec<Message>,
    config: &AgentConfig,
) -> AgentResult {
    let mut messages = initial_messages;
    let mut total_usage = Usage::default();

    loop {
        // Safe boundary: no provider stream or tool call is active here. This
        // runs before the initial generation and after complete tool-result
        // batches; the notice is ephemeral system context, not fake history.
        let coordination_notice = coordination_model_notice(&session).await;
        let system = match (config.system.as_deref(), coordination_notice.as_deref()) {
            (Some(base), Some(notice)) => Some(format!("{base}\n\n{notice}")),
            (None, Some(notice)) => Some(notice.to_string()),
            (Some(base), None) => Some(base.to_string()),
            (None, None) => None,
        };
        let ctx = Context {
            messages: messages.clone(),
            system,
            tools: config.tools.clone(),
            stable_prefix_len: 0,
        };

        let ordinal = config
            .generation_ordinal
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let generation =
            crate::observability::GenerationSpan::new(crate::observability::GenerationMetadata {
                kind: "agent",
                model: &config.opts.model,
                max_tokens: config.opts.max_tokens,
                thinking: config.opts.thinking.clone(),
                temperature: config.opts.temperature,
                ordinal,
                tools_exposed: ctx.tools.len(),
                stable_prefix_len: ctx.stable_prefix_len,
            });
        let resp = match &config.on_stream_event {
            Some(hook) => {
                provider
                    .stream(&ctx, &config.opts, &mut |ev| {
                        generation.mark_first_token();
                        hook(ev);
                    })
                    .instrument(generation.span().clone())
                    .await
            }
            None => {
                provider
                    .stream(&ctx, &config.opts, &mut |_| {
                        generation.mark_first_token();
                    })
                    .instrument(generation.span().clone())
                    .await
            }
        };
        generation.finish(&resp);
        total_usage = accumulate_usage(total_usage, resp.usage.clone());
        if let Some(log_cfg) = &config.token_log {
            log_token_usage(log_cfg, &config.opts.model, &resp.usage);
        }

        // Assistant turn appended BEFORE tool results (Anthropic API requirement)
        messages.push(Message {
            role: Role::Assistant,
            content: resp.content.clone(),
        });

        match resp.stop_reason {
            StopReason::EndTurn
            | StopReason::MaxTokens
            | StopReason::Refusal
            | StopReason::Aborted
            | StopReason::Error => {
                return AgentResult {
                    messages,
                    usage: total_usage,
                    stop_reason: resp.stop_reason,
                    error_message: resp.error_message,
                    last_call_usage: resp.usage,
                    context_overflow: resp.context_overflow,
                };
            }
            StopReason::ToolUse => {
                let calls: Vec<_> = resp
                    .content
                    .iter()
                    .filter_map(|b| {
                        if let ContentBlock::ToolCall { id, name, input } = b {
                            Some((id.clone(), name.clone(), input.clone()))
                        } else {
                            None
                        }
                    })
                    .collect();

                let mut tool_results = Vec::new();
                let mut terminate = false;

                for (id, name, input) in calls {
                    let info = ToolCallInfo {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    };
                    // before_tool_call hook
                    if let Some(hook) = &config.before_tool_call {
                        match hook(&info).await {
                            BeforeHookResult::Allow => {}
                            BeforeHookResult::Block(reason) => {
                                crate::observability::ToolSpan::new(
                                    &name,
                                    dispatch_tool_kind(&name),
                                )
                                .finish_status(crate::observability::ToolStatus::Blocked);
                                tool_results.push(ContentBlock::ToolResult {
                                    tool_use_id: id,
                                    content: format!("blocked: {reason}"),
                                    is_error: true,
                                });
                                continue;
                            }
                        }
                    }

                    // invoke via facade
                    let on_progress = |event| {
                        if let Some(hook) = &config.on_tool_progress {
                            hook(&info, event);
                        }
                    };
                    let progress_callback = config
                        .on_tool_progress
                        .as_ref()
                        .map(|_| &on_progress as &crate::ops::ExecProgressCallback<'_>);
                    let request_chars = serde_json::to_string(&input).map(|s| s.len()).unwrap_or(0);
                    // Native/opcode tools and the plan tool run through the
                    // facade; open a `tool.call` span around them so latency is
                    // measured. Remote MCP tools are spanned as `mcp.remote_tool`
                    // in the bridge; anything that resolves to neither is
                    // recorded below as `unavailable`.
                    // Tools dispatched inside this loop (opcode/native via the
                    // facade, the plan tool, and execute_script) get a
                    // `tool.call` span. Remote MCP tools are spanned as
                    // `mcp.remote_tool` in the bridge; anything resolving to
                    // neither is recorded below as `unavailable`.
                    let dispatched = name == UPDATE_PLAN_TOOL
                        || name == EXECUTE_SCRIPT_TOOL
                        || crate::tools::has_opcode_mapping(&name);
                    let tool_span = dispatched.then(|| {
                        crate::observability::ToolSpan::new(
                            &name,
                            crate::observability::tool_kind(&name),
                        )
                    });
                    let (content, is_error, outcome) = if name == EXECUTE_SCRIPT_TOOL {
                        // execute_script shares the live session with its
                        // Starlark sandbox thread, so the loop hands it an
                        // `Arc<Mutex<Session>>` clone rather than a `&mut`.
                        let code = input
                            .get("code")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let timeout_secs = input
                            .get("timeout")
                            .and_then(serde_json::Value::as_i64)
                            // Non-positive → default (also guards a negative
                            // value wrapping to a huge u64); clamp the upper
                            // bound to the safety ceiling.
                            .filter(|secs| *secs > 0)
                            .map(|secs| secs.min(MAX_SCRIPT_TIMEOUT_SECS))
                            .unwrap_or(60) as u64;
                        let cfg = {
                            let mut guard = session.lock().await;
                            guard.used_tools.insert(EXECUTE_SCRIPT_TOOL.to_string());
                            std::sync::Arc::clone(&guard.cfg)
                        };
                        // Accumulates the spend of any in-script LLM sub-calls
                        // (ADR-008), read back after the run and folded into the
                        // turn total. Shared with the sandbox `SubcallEnv`.
                        let subcall_usage =
                            std::sync::Arc::new(std::sync::Mutex::new(Usage::default()));
                        // Sub-calls are available only when a provider was
                        // injected (agent/chat/ACP mode) AND the operator opted
                        // in via `process.script_llm_enabled`. `None` leaves the
                        // `llm_query*` builtins raising a clear error.
                        let subcall = config.subcall_provider.as_ref().and_then(|provider| {
                            cfg.process
                                .script_llm_enabled
                                .then(|| crate::script::SubcallEnv {
                                    provider: std::sync::Arc::clone(provider),
                                    model: config.opts.model.clone(),
                                    max_tokens: config.opts.max_tokens,
                                    max_subcalls: cfg.process.max_script_subcalls,
                                    max_batch: cfg.process.max_script_subcall_batch,
                                    usage: std::sync::Arc::clone(&subcall_usage),
                                    count: std::sync::Arc::new(
                                        std::sync::atomic::AtomicUsize::new(0),
                                    ),
                                    ordinal: std::sync::Arc::clone(&config.generation_ordinal),
                                })
                        });
                        // Caller-owned op counter so batch_size reflects the ops
                        // that ran even when the script errors mid-run.
                        let op_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                        let script_fut = crate::script::execute_with_op_count(
                            &code,
                            std::sync::Arc::clone(&session),
                            std::time::Duration::from_secs(timeout_secs),
                            std::sync::Arc::clone(&op_count),
                            subcall,
                        );
                        let script_result = match &tool_span {
                            Some(span) => script_fut.instrument(span.span().clone()).await,
                            None => script_fut.await,
                        };
                        // Fold any in-script LLM sub-call spend into the turn
                        // total (ADR-008). A no-op when no sub-calls ran.
                        // Recover the accumulated spend even if the sandbox
                        // thread panicked while holding the lock, rather than
                        // silently dropping it to zero.
                        let subcall_spend = match subcall_usage.lock() {
                            Ok(spend) => spend.clone(),
                            Err(poisoned) => poisoned.into_inner().clone(),
                        };
                        total_usage = accumulate_usage(total_usage, subcall_spend);
                        let (content, is_error) = match script_result {
                            Ok(result) => {
                                let mut response = serde_json::json!({ "result": result.value });
                                if !result.logs.is_empty() {
                                    response["logs"] = serde_json::json!(result.logs);
                                }
                                (response.to_string(), false)
                            }
                            Err(error) => (error, true),
                        };
                        let outcome = ToolOutcome {
                            request_tokens_est: crate::analytics::estimate_tokens(request_chars),
                            response_tokens_est: crate::analytics::estimate_tokens(content.len()),
                            // Child-op count (D5); read after the run so it holds
                            // even on a mid-run script error.
                            batch_size: op_count.load(std::sync::atomic::Ordering::Relaxed) as u64,
                            ..ToolOutcome::default()
                        };
                        (content, is_error, Some(outcome))
                    } else if name == UPDATE_PLAN_TOOL {
                        let (content, is_error) = match parse_plan_entries(&input) {
                            Ok(entries) => {
                                if let Some(hook) = &config.on_plan_update {
                                    hook(&entries);
                                }
                                (
                                    serde_json::json!({"updated": entries.len()}).to_string(),
                                    false,
                                )
                            }
                            Err(error) => (error, true),
                        };
                        let outcome = ToolOutcome {
                            request_tokens_est: crate::analytics::estimate_tokens(request_chars),
                            batch_size: 1,
                            ..ToolOutcome::default()
                        };
                        (content, is_error, Some(outcome))
                    } else {
                        // A native tool needs exclusive `&mut Session` for its
                        // whole execution, so the guard is held across the call.
                        // That starves no one: tool calls in a turn are
                        // sequential and each session's `run` is serialized by
                        // the frontend, so nothing else (including the sandbox
                        // thread, which only runs inside the execute_script
                        // branch of another iteration) contends for it here.
                        // Locking per call — not for the whole turn — is what
                        // lets that later execute_script branch acquire it.
                        let facade = {
                            let mut session_guard = session.lock().await;
                            match &tool_span {
                                Some(span) => {
                                    tool_facade::invoke_with_progress(
                                        &mut session_guard,
                                        &name,
                                        &input,
                                        progress_callback,
                                    )
                                    .instrument(span.span().clone())
                                    .await
                                }
                                None => {
                                    tool_facade::invoke_with_progress(
                                        &mut session_guard,
                                        &name,
                                        &input,
                                        progress_callback,
                                    )
                                    .await
                                }
                            }
                        };
                        match facade {
                            Some(r) => {
                                let ok = r.ok;
                                let meta = r.meta.clone();
                                let content = response_to_content(r);
                                let response_chars = content.len();
                                let content = if name == LIST_ALL_TOOLS_TOOL {
                                    append_remote_tools_to_catalog(content, &config.tools)
                                } else {
                                    content
                                };
                                let (saved_tokens, _) = crate::analytics::compute_savings(
                                    meta.unfiltered_chars,
                                    response_chars,
                                );
                                let outcome = ToolOutcome {
                                    request_tokens_est: crate::analytics::estimate_tokens(
                                        request_chars,
                                    ),
                                    response_tokens_est: crate::analytics::estimate_tokens(
                                        response_chars,
                                    ),
                                    saved_tokens_est: saved_tokens,
                                    redirect: meta.redirect_via_plugin,
                                    filtered: meta.filter_applied,
                                    read_dedup: meta.read_dedup,
                                    batch_size: 1,
                                };
                                (content, !ok, Some(outcome))
                            }
                            None => {
                                let served = match &config.remote_tool_dispatch {
                                    Some(hook) => hook(&name, &input).await,
                                    None => None,
                                };
                                match served {
                                    // Remote MCP: the bridge emits the
                                    // `mcp.remote_tool` span with its server alias.
                                    Some(r) => (r.content, r.is_error, None),
                                    None => {
                                        // Emit a standalone span only when no
                                        // `tool_span` is open for this call
                                        // (i.e. a non-native tool). Guarding on
                                        // `tool_span` avoids double-spanning
                                        // should an opcode tool ever reach here.
                                        if tool_span.is_none() {
                                            crate::observability::ToolSpan::new(
                                                &name,
                                                dispatch_tool_kind(&name),
                                            )
                                            .finish_status(
                                                crate::observability::ToolStatus::Unavailable,
                                            );
                                        }
                                        (
                                            format!("tool '{name}' not available in agent mode"),
                                            true,
                                            None,
                                        )
                                    }
                                }
                            }
                        }
                    };
                    if let Some(tool_span) = tool_span {
                        let status = if is_error {
                            crate::observability::ToolStatus::Error
                        } else {
                            crate::observability::ToolStatus::Success
                        };
                        tool_span.finish(status, outcome.unwrap_or_default());
                    }

                    // after_tool_call hook
                    if let Some(hook) = &config.after_tool_call {
                        if matches!(hook(&info, &content, is_error), AfterHookResult::Terminate) {
                            terminate = true;
                        }
                    }

                    tool_results.push(ContentBlock::ToolResult {
                        tool_use_id: id,
                        content,
                        is_error,
                    });
                }

                if !tool_results.is_empty() {
                    messages.push(Message {
                        role: Role::User,
                        content: tool_results,
                    });
                }

                if terminate {
                    return AgentResult {
                        messages,
                        usage: total_usage,
                        stop_reason: StopReason::Aborted,
                        error_message: Some("terminated by after_tool_call hook".to_string()),
                        last_call_usage: resp.usage,
                        context_overflow: false,
                    };
                }
            }
        }
    }
}

// --- Stateful multi-turn session (project #183, task #956) ---

/// One turn's outcome from [`AgentSession::prompt`].
pub struct TurnResult {
    /// Concatenated text of the final assistant message this turn.
    pub text: String,
    /// Token/cost usage for THIS turn.
    pub usage: Usage,
    /// Usage from the turn's final provider request. Unlike `usage`, this is
    /// one context snapshot rather than the sum of every tool-loop request.
    pub last_call_usage: Usage,
    pub stop_reason: StopReason,
    pub error_message: Option<String>,
    pub context_overflow: bool,
}

/// A stateful, re-promptable agent conversation wrapping the one-shot [`run`]
/// loop: holds the provider, the tool `Session`, the loop config (incl. the
/// safety hook), and the running message history + accumulated usage. Shared
/// core for the REPL and ACP frontends (project #183).
pub struct AgentSession {
    /// `Arc` (not `Box`) so it can be cloned into `AgentConfig.subcall_provider`
    /// for in-script LLM sub-calls (ADR-008).
    provider: std::sync::Arc<dyn LlmProvider>,
    /// Shared with `execute_script`'s Starlark sandbox thread, so it is an
    /// `Arc<Mutex<Session>>` rather than an owned `Session`. The tool loop
    /// locks it per native call and clones the `Arc` for script execution.
    tool_session: std::sync::Arc<tokio::sync::Mutex<Session>>,
    config: AgentConfig,
    messages: Vec<Message>,
    total_usage: Usage,
    /// The last API call's measured window occupancy
    /// (`Usage::prompt_tokens()`), read by the proactive compaction trigger
    /// (ADR-002). 0 = not yet measured (fresh/resumed session, or the
    /// provider returned no usage) — the trigger falls back to a chars/4
    /// estimate then.
    last_prompt_tokens: u64,
}

impl AgentSession {
    pub fn new(
        provider: Box<dyn LlmProvider>,
        tool_session: Session,
        mut config: AgentConfig,
    ) -> Self {
        let provider: std::sync::Arc<dyn LlmProvider> = std::sync::Arc::from(provider);
        // Expose the provider to in-script LLM sub-calls (ADR-008); the sandbox
        // reads it from the config on the execute_script path.
        config.subcall_provider = Some(std::sync::Arc::clone(&provider));
        AgentSession {
            provider,
            tool_session: std::sync::Arc::new(tokio::sync::Mutex::new(tool_session)),
            config,
            messages: Vec::new(),
            total_usage: Usage::default(),
            last_prompt_tokens: 0,
        }
    }

    /// Send a user message, run the tool loop to completion, and return this
    /// turn's assistant text + usage. History and accumulated usage persist for
    /// the next prompt.
    ///
    /// Cancel-safe: `self.messages` is only overwritten after `run` completes,
    /// so dropping this future mid-await (e.g. a REPL Ctrl-C abort) leaves the
    /// session's history untouched instead of losing it to a half-finished turn.
    pub async fn prompt(&mut self, user_text: impl Into<String>) -> TurnResult {
        self.prompt_message(Message::user(user_text)).await
    }

    /// Multimodal form of [`Self::prompt`]. The caller supplies a complete
    /// user-role message so ACP images and embedded context survive provider
    /// serialization, history persistence, and retries.
    pub async fn prompt_message(&mut self, user_message: Message) -> TurnResult {
        debug_assert_eq!(user_message.role, Role::User);
        self.config
            .generation_ordinal
            .store(0, std::sync::atomic::Ordering::Relaxed);

        // Proactive compaction (ADR-002): compact BEFORE the turn when the
        // last measured occupancy (or, if never measured, an estimate)
        // crossed the high-water mark.
        if let Some(policy) = &self.config.compaction {
            let occupancy = if self.last_prompt_tokens > 0 {
                self.last_prompt_tokens
            } else {
                compaction::estimate_prompt_tokens(self.config.system.as_deref(), &self.messages)
            };
            if policy.should_compact(occupancy) {
                self.compact("proactive").await;
            }
        }

        let result = self.attempt(&user_message).await;

        // Reactive safety net (ADR-002): a classified context overflow means
        // our between-turns measurement missed — compact and retry ONCE.
        // The failed attempt is not committed, so the retry runs against the
        // freshly compacted history.
        let result = if result.context_overflow && self.config.compaction.is_some() {
            // Ordinal of the generation that overflowed — links the retry to
            // the failed generation (ADR-006 D5).
            let failed_ordinal = self
                .config
                .generation_ordinal
                .load(std::sync::atomic::Ordering::Relaxed)
                .checked_sub(1);
            if self.compact("reactive_overflow").await.is_some() {
                let retry =
                    crate::observability::RetrySpan::new("context_overflow", failed_ordinal);
                let retried = self
                    .attempt(&user_message)
                    .instrument(retry.span().clone())
                    .await;
                retry.finish(retry_error_type(&retried));
                retried
            } else {
                result
            }
        } else {
            result
        };

        self.commit(result)
    }

    /// Run one turn attempt against the current history without committing
    /// its messages. Usage and the measured occupancy are recorded either
    /// way (a failed attempt still spent/observed them).
    async fn attempt(&mut self, user_message: &Message) -> AgentResult {
        self.attempt_with_history(self.messages.clone(), user_message)
            .await
    }

    async fn attempt_with_history(
        &mut self,
        mut history: Vec<Message>,
        user_message: &Message,
    ) -> AgentResult {
        history.push(user_message.clone());
        let result = run(
            self.provider.as_ref(),
            std::sync::Arc::clone(&self.tool_session),
            history,
            &self.config,
        )
        .await;
        self.total_usage =
            accumulate_usage(std::mem::take(&mut self.total_usage), result.usage.clone());
        self.last_prompt_tokens = result.last_call_usage.prompt_tokens();
        result
    }

    pub async fn retry_last_turn(&mut self) -> Result<TurnResult, String> {
        let Some(user_index) = self.messages.iter().rposition(is_user_turn_message) else {
            return Err("cannot retry: session has no user turn".to_string());
        };
        let user_message = self.messages[user_index].clone();
        let base_history = self.messages[..user_index].to_vec();
        let retry = crate::observability::RetrySpan::new("explicit", None);
        let result = self
            .attempt_with_history(base_history, &user_message)
            .instrument(retry.span().clone())
            .await;
        retry.finish(retry_error_type(&result));
        Ok(self.commit(result))
    }

    /// Poll unread agent mail for a human-visible ACP notice. Called by the
    /// ACP background task only; acquiring the AgentSession lock means this
    /// cannot run during a provider stream or tool execution. Deduplicated by
    /// a UI-specific newest-message watermark. Metadata only, fail-open.
    pub fn coordination_tool_session(&self) -> std::sync::Arc<tokio::sync::Mutex<Session>> {
        std::sync::Arc::clone(&self.tool_session)
    }

    pub async fn poll_coordination_ui_notice(
        tool_session: &std::sync::Arc<tokio::sync::Mutex<Session>>,
    ) -> Option<(String, i64)> {
        let (agent, watermark, db_path, busy_timeout) = {
            let s = tool_session.lock().await;
            let cfg = &s.cfg.coordination;
            if !cfg.enabled || !cfg.notifications.enabled || !cfg.notifications.ui_notice {
                return None;
            }
            (
                s.coordination_agent_name.clone()?,
                s.coordination_ui_watermark,
                crate::coordination::workspace_db_path(&cfg.resolved_db_dir(), &s.workspace),
                cfg.effective_busy_timeout_ms(),
            )
        };
        let agent_for_query = agent.clone();
        let summary = match tokio::task::spawn_blocking(move || {
            crate::coordination::CoordinationStore::open_with(&db_path, busy_timeout)
                .and_then(|store| store.unread_summary(&agent_for_query, watermark))
        })
        .await
        {
            Ok(Ok(summary)) => summary?,
            Ok(Err(error)) => {
                tracing::warn!(target: "daimonos::coordination", event="ui_notification_check_failed", error=%error);
                return None;
            }
            Err(error) => {
                tracing::warn!(target: "daimonos::coordination", event="ui_notification_task_failed", error=%error);
                return None;
            }
        };
        Some((
            format!(
                "Agent mail: {} new unread message(s) for {} (highest importance: {}). The agent will be notified at its next safe action boundary.",
                summary.count, agent, summary.highest_importance
            ),
            summary.newest_message_id,
        ))
    }

    /// Advance UI dedup only after ACP delivery was attempted with a live
    /// connection. A stale/lost candidate cannot suppress a later notice.
    pub async fn acknowledge_coordination_ui_notice(
        tool_session: &std::sync::Arc<tokio::sync::Mutex<Session>>,
        newest_message_id: i64,
    ) {
        let mut s = tool_session.lock().await;
        s.coordination_ui_watermark = s.coordination_ui_watermark.max(newest_message_id);
    }

    pub fn user_turn_count(&self) -> usize {
        self.messages
            .iter()
            .filter(|message| is_user_turn_message(message))
            .count()
    }

    pub fn truncate_from_user_turn(&mut self, turn_index: usize) -> Result<(), String> {
        let Some(history_index) = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| is_user_turn_message(message))
            .nth(turn_index)
            .map(|(index, _)| index)
        else {
            return Err(format!("user turn index {turn_index} not found"));
        };
        let evicted_messages = self.messages.len() - history_index;
        let evicted_turns = self.messages[history_index..]
            .iter()
            .filter(|message| is_user_turn_message(message))
            .count();
        self.messages.truncate(history_index);
        self.last_prompt_tokens = 0;
        crate::observability::record_truncation(
            turn_index as u64,
            evicted_turns as u64,
            evicted_messages as u64,
        );
        Ok(())
    }

    /// Commit an attempt's outcome as this turn's result.
    fn commit(&mut self, result: AgentResult) -> TurnResult {
        let text = last_assistant_text(&result.messages);
        self.messages = result.messages;
        TurnResult {
            text,
            usage: result.usage,
            last_call_usage: result.last_call_usage,
            stop_reason: result.stop_reason,
            error_message: result.error_message,
            context_overflow: result.context_overflow,
        }
    }

    /// Compact the history per the configured policy (ADR-002): evict the
    /// oldest turns at a turn boundary, replace them with one summary
    /// message produced by a deterministic one-shot LLM call (retried once;
    /// structural drop with a marker on repeated failure). Returns `None`
    /// when compaction is off or nothing is evictable.
    async fn compact(&mut self, trigger: &str) -> Option<CompactionEvent> {
        let policy = self.config.compaction.as_ref()?;
        let cut = compaction::choose_cut(&self.messages, policy.target_tokens())?;
        let summary_model = policy
            .summary_model
            .clone()
            .unwrap_or_else(|| self.config.opts.model.clone());
        let summary_system = policy
            .summary_prompt
            .clone()
            .unwrap_or_else(compaction::default_summary_prompt);
        // Snapshot policy thresholds and the occupancy that triggered this
        // compaction for the span before the `policy` borrow is released.
        let high_water = policy.high_water;
        let low_water = policy.low_water;
        let occupancy_tokens = if self.last_prompt_tokens > 0 {
            self.last_prompt_tokens
        } else {
            compaction::estimate_prompt_tokens(self.config.system.as_deref(), &self.messages)
        };

        let est_tokens_before = compaction::estimate_tokens(&self.messages);
        let evicted = &self.messages[..cut];
        let evicted_turns = compaction::turn_starts(evicted).len();
        let transcript = compaction::transcript_for_summary(evicted);

        // context.compaction span (ADR-006 D4): the summary generation below
        // is instrumented under it so the llm.generation nests beneath.
        let compaction_span =
            crate::observability::CompactionSpan::new(crate::observability::CompactionMetadata {
                trigger,
                strategy: CompactionStrategy::Summarize.as_str(),
                high_water,
                low_water,
                occupancy_tokens,
                summary_model: &summary_model,
            });

        // One-shot, tool-free, deterministic summarization call. Text-in/
        // text-out so the request itself is never subject to tool-pair
        // validity (see compaction::transcript_for_summary).
        let opts = CompleteOpts {
            model: summary_model.clone(),
            max_tokens: self.config.opts.max_tokens,
            thinking: ThinkingLevel::Off,
            temperature: Some(0.0),
        };
        let ctx = Context {
            messages: vec![Message::user(format!(
                "Summarize this earlier conversation:\n\n{transcript}"
            ))],
            system: Some(summary_system),
            tools: vec![],
            stable_prefix_len: 0,
        };

        // One attempt + one retry (ADR-002 Q4); its tokens count toward the
        // session's cumulative usage like any other call. The whole loop is
        // instrumented under the compaction span so each summary
        // llm.generation nests beneath context.compaction (D4).
        let mut summary_text: Option<String> = None;
        let mut summary_attempts: u64 = 0;
        async {
            for _ in 0..2 {
                summary_attempts += 1;
                let ordinal = self
                    .config
                    .generation_ordinal
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let generation = crate::observability::GenerationSpan::new(
                    crate::observability::GenerationMetadata {
                        kind: "compaction_summary",
                        model: &opts.model,
                        max_tokens: opts.max_tokens,
                        thinking: opts.thinking.clone(),
                        temperature: opts.temperature,
                        ordinal,
                        tools_exposed: 0,
                        stable_prefix_len: 0,
                    },
                );
                let resp = self
                    .provider
                    .complete(&ctx, &opts)
                    .instrument(generation.span().clone())
                    .await;
                generation.finish(&resp);
                self.total_usage =
                    accumulate_usage(std::mem::take(&mut self.total_usage), resp.usage.clone());
                if let Some(log_cfg) = &self.config.token_log {
                    log_token_usage(log_cfg, &summary_model, &resp.usage);
                }
                if resp.error_message.is_none() {
                    let text: String = resp
                        .content
                        .iter()
                        .filter_map(|b| {
                            if let ContentBlock::Text(t) = b {
                                Some(t.as_str())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.trim().is_empty() {
                        summary_text = Some(text.trim().to_string());
                        break;
                    }
                }
            }
        }
        .instrument(compaction_span.span().clone())
        .await;

        let fallback_drop = summary_text.is_none();
        let replacement = match &summary_text {
            Some(text) => compaction::summary_message(text),
            None => compaction::drop_marker_message(),
        };
        let mut compacted = Vec::with_capacity(1 + self.messages.len() - cut);
        compacted.push(replacement);
        compacted.extend_from_slice(&self.messages[cut..]);
        self.messages = compacted;
        // The measured occupancy no longer describes the compacted history;
        // the next proactive check re-estimates.
        self.last_prompt_tokens = 0;

        let est_tokens_after = compaction::estimate_tokens(&self.messages);
        compaction_span.finish(crate::observability::CompactionOutcome {
            tokens_before_est: est_tokens_before,
            tokens_after_est: est_tokens_after,
            evicted_turns: evicted_turns as u64,
            evicted_messages: cut as u64,
            summary_retries: summary_attempts.saturating_sub(1),
            fallback_drop,
        });
        let event = CompactionEvent {
            evicted_turns,
            evicted_messages: cut,
            est_tokens_before,
            est_tokens_after,
            summary_model,
            strategy: CompactionStrategy::Summarize,
            fallback_drop,
        };
        if let Some(log_cfg) = &self.config.token_log {
            log_compaction_event(log_cfg, &event);
        }
        if let Some(hook) = &self.config.on_compaction {
            hook(&event);
        }
        Some(event)
    }

    /// Full conversation history so far.
    pub fn history(&self) -> &[Message] {
        &self.messages
    }

    /// Replace the conversation history, e.g. restoring a persisted ACP
    /// session on `session/load` after a process restart. Cumulative usage is
    /// left untouched (it's a fresh process, so there's none to preserve).
    pub fn set_history(&mut self, messages: Vec<Message>) {
        self.messages = messages;
    }

    /// Usage accumulated across every turn this session.
    pub fn total_usage(&self) -> &Usage {
        &self.total_usage
    }

    /// Reset the conversation (e.g. REPL `/clear`); cumulative usage is kept.
    pub fn clear(&mut self) {
        self.messages.clear();
    }

    /// Switch the model used for subsequent turns (e.g. the ACP model
    /// picker, vikunja #960). History and cumulative usage are preserved —
    /// only the model sent on the next `prompt` changes.
    pub fn set_model(&mut self, model: impl Into<String>) {
        self.config.opts.model = model.into();
    }

    /// Replace the policy used for subsequent turns. ACP uses this when its
    /// model picker switches between models with different context windows.
    pub fn set_compaction(&mut self, policy: Option<CompactionPolicy>) {
        self.config.compaction = policy;
    }

    /// Replace the schemas sent on subsequent provider calls. ACP uses this
    /// after refreshing its forwarded MCP bridge for a live session.
    pub fn set_tools(&mut self, tools: Vec<ToolSchema>) {
        self.config.tools = tools;
    }

    /// Ask this session's provider for a model's current context-window size.
    pub async fn context_window(&self, model: &str) -> Option<u64> {
        self.provider.context_window(model).await
    }

    /// The model currently configured for this session.
    pub fn model(&self) -> &str {
        &self.config.opts.model
    }

    pub fn tool_count(&self) -> usize {
        self.config.tools.len()
    }
}

fn is_user_turn_message(message: &Message) -> bool {
    if message.role != Role::User {
        return false;
    }
    message.content.iter().any(|block| match block {
        ContentBlock::Text(text) => {
            !text.starts_with("[Summary of earlier conversation:")
                && !text.starts_with("[Earlier conversation truncated")
        }
        ContentBlock::Image { .. } => true,
        ContentBlock::ToolResult { .. }
        | ContentBlock::ToolCall { .. }
        | ContentBlock::Thinking(_) => false,
    })
}

/// Concatenate the `Text` blocks of the last assistant message in `messages`.
fn last_assistant_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::providers::LlmResponse;
    use async_trait::async_trait;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    // --- MockProvider ---

    struct MockProvider {
        responses: Mutex<VecDeque<LlmResponse>>,
    }

    impl MockProvider {
        fn new(responses: Vec<LlmResponse>) -> Self {
            MockProvider {
                responses: Mutex::new(VecDeque::from(responses)),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        async fn complete(&self, _ctx: &Context, _opts: &CompleteOpts) -> LlmResponse {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| LlmResponse::error("MockProvider exhausted"))
        }
    }

    struct CaptureProvider {
        seen: Arc<Mutex<Vec<Message>>>,
    }

    #[async_trait]
    impl LlmProvider for CaptureProvider {
        async fn complete(&self, ctx: &Context, _opts: &CompleteOpts) -> LlmResponse {
            *self.seen.lock().unwrap() = ctx.messages.clone();
            end_turn_resp()
        }
    }

    fn mock_usage(input: u64, output: u64) -> Usage {
        Usage {
            input,
            output,
            ..Usage::default()
        }
    }

    fn end_turn_resp() -> LlmResponse {
        end_turn_resp_with_text("done")
    }

    fn end_turn_resp_with_text(text: &str) -> LlmResponse {
        LlmResponse {
            content: vec![ContentBlock::Text(text.to_string())],
            stop_reason: StopReason::EndTurn,
            error_message: None,
            context_overflow: false,
            usage: mock_usage(100, 50),
        }
    }

    fn tool_call_resp(id: &str, name: &str, input: Value) -> LlmResponse {
        LlmResponse {
            content: vec![ContentBlock::ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                input,
            }],
            stop_reason: StopReason::ToolUse,
            error_message: None,
            context_overflow: false,
            usage: mock_usage(200, 100),
        }
    }

    fn session_in(dir: &std::path::Path) -> Session {
        Session::new(dir.to_path_buf(), Arc::new(Config::default()))
    }

    /// Wrap a session for `run`, which now shares it with execute_script's
    /// sandbox thread via `Arc<Mutex<Session>>` (vikunja #1050).
    fn shared(session: Session) -> Arc<tokio::sync::Mutex<Session>> {
        Arc::new(tokio::sync::Mutex::new(session))
    }

    #[tokio::test]
    async fn coordination_model_notice_is_metadata_only_and_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        let db_dir = dir.path().join("coord-db");
        let mut cfg = Config::default();
        cfg.coordination.db_dir = Some(db_dir.to_string_lossy().to_string());
        let mut session = Session::new(dir.path().to_path_buf(), Arc::new(cfg));
        session.coordination_agent_name = Some("BlueLake".to_string());
        let db = crate::coordination::workspace_db_path(&db_dir, dir.path());
        let store = crate::coordination::CoordinationStore::open_with(&db, 5_000).unwrap();
        store
            .send_message(
                "RedStone",
                &["BlueLake".into()],
                &[],
                "SUBJECT_SECRET",
                "BODY_SECRET",
                crate::coordination::Importance::High,
                false,
                None,
                "2026-07-25T00:00:00Z",
            )
            .unwrap();
        let shared = shared(session);
        let notice = coordination_model_notice(&shared).await.unwrap();
        assert!(notice.contains("1 new unread"));
        assert!(notice.contains("Highest importance: high"));
        assert!(notice.contains("fetch_inbox"));
        assert!(!notice.contains("SUBJECT_SECRET"));
        assert!(!notice.contains("BODY_SECRET"));
        assert!(coordination_model_notice(&shared).await.is_none());
    }

    #[tokio::test]
    async fn mail_arriving_during_tool_call_is_visible_only_next_generation() {
        let dir = tempfile::tempdir().unwrap();
        let db_dir = dir.path().join("coord-db");
        let mut cfg = Config::default();
        cfg.coordination.db_dir = Some(db_dir.to_string_lossy().to_string());
        let mut session = Session::new(dir.path().to_path_buf(), Arc::new(cfg));
        session.coordination_agent_name = Some("BlueLake".to_string());
        let db = crate::coordination::workspace_db_path(&db_dir, dir.path());
        let sent = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sent_hook = Arc::clone(&sent);
        let db_hook = db.clone();
        let config = AgentConfig {
            after_tool_call: Some(Box::new(move |_, _, _| {
                let store =
                    crate::coordination::CoordinationStore::open_with(&db_hook, 5_000).unwrap();
                store
                    .send_message(
                        "RedStone",
                        &["BlueLake".into()],
                        &[],
                        "SECRET_SUBJECT",
                        "SECRET_BODY",
                        crate::coordination::Importance::High,
                        false,
                        None,
                        "2026-07-25T00:00:00Z",
                    )
                    .unwrap();
                sent_hook.store(true, std::sync::atomic::Ordering::SeqCst);
                AfterHookResult::Continue
            })),
            ..AgentConfig::default()
        };
        let provider = RecordingProvider::new(vec![
            tool_call_resp("t1", "session_stats", json!({})),
            end_turn_resp(),
        ]);
        let calls = provider.calls_handle();
        let result = run(
            &provider,
            shared(session),
            vec![Message::user("go")],
            &config,
        )
        .await;
        assert!(sent.load(std::sync::atomic::Ordering::SeqCst));
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(!calls[0]
            .2
            .as_deref()
            .unwrap_or_default()
            .contains("COORDINATION NOTICE"));
        let second_system = calls[1].2.as_deref().unwrap_or_default();
        assert!(second_system.contains("DAIMONOS COORDINATION NOTICE"));
        assert!(!second_system.contains("SECRET_SUBJECT"));
        assert!(!second_system.contains("SECRET_BODY"));
        // Ephemeral notice is not persisted as a fake user message.
        assert!(!result.messages.iter().any(|m| m.content.iter().any(|b| {
            matches!(b, ContentBlock::Text(t) if t.contains("DAIMONOS COORDINATION NOTICE"))
        })));
    }

    #[tokio::test]
    async fn ui_notice_is_metadata_only_and_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        let db_dir = dir.path().join("coord-db");
        let mut cfg = Config::default();
        cfg.coordination.db_dir = Some(db_dir.to_string_lossy().to_string());
        let mut tool_session = Session::new(dir.path().to_path_buf(), Arc::new(cfg));
        tool_session.coordination_agent_name = Some("BlueLake".to_string());
        let db = crate::coordination::workspace_db_path(&db_dir, dir.path());
        let store = crate::coordination::CoordinationStore::open_with(&db, 5_000).unwrap();
        store
            .send_message(
                "RedStone",
                &["BlueLake".into()],
                &[],
                "SECRET_SUBJECT",
                "SECRET_BODY",
                crate::coordination::Importance::Urgent,
                false,
                None,
                "2026-07-25T00:00:00Z",
            )
            .unwrap();
        let sess = AgentSession::new(
            Box::new(MockProvider::new(vec![])),
            tool_session,
            AgentConfig::default(),
        );
        let tool_session = sess.coordination_tool_session();
        let (notice, newest) = AgentSession::poll_coordination_ui_notice(&tool_session)
            .await
            .unwrap();
        assert!(notice.contains("highest importance: urgent"));
        assert!(!notice.contains("SECRET_SUBJECT"));
        assert!(!notice.contains("SECRET_BODY"));
        // Candidate repeats until delivery is acknowledged.
        assert!(AgentSession::poll_coordination_ui_notice(&tool_session)
            .await
            .is_some());
        AgentSession::acknowledge_coordination_ui_notice(&tool_session, newest).await;
        assert!(AgentSession::poll_coordination_ui_notice(&tool_session)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn agent_session_prompt_delivers_model_notice() {
        let dir = tempfile::tempdir().unwrap();
        let db_dir = dir.path().join("coord-db");
        let mut cfg = Config::default();
        cfg.coordination.db_dir = Some(db_dir.to_string_lossy().to_string());
        let mut tool_session = Session::new(dir.path().to_path_buf(), Arc::new(cfg));
        tool_session.coordination_agent_name = Some("BlueLake".to_string());
        let db = crate::coordination::workspace_db_path(&db_dir, dir.path());
        let store = crate::coordination::CoordinationStore::open_with(&db, 5_000).unwrap();
        store
            .send_message(
                "RedStone",
                &["BlueLake".into()],
                &[],
                "secret",
                "secret body",
                crate::coordination::Importance::Normal,
                false,
                None,
                "2026-07-25T00:00:00Z",
            )
            .unwrap();
        let provider = RecordingProvider::new(vec![end_turn_resp()]);
        let calls = provider.calls_handle();
        let mut sess = AgentSession::new(Box::new(provider), tool_session, AgentConfig::default());
        sess.prompt("work").await;
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let system = calls[0].2.as_deref().unwrap_or_default();
        assert!(system.contains("DAIMONOS COORDINATION NOTICE"));
        assert!(!system.contains("secret body"));
    }

    #[tokio::test]
    async fn coordination_notices_require_registered_identity() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(session_in(dir.path()));
        assert!(coordination_model_notice(&shared).await.is_none());
    }

    // --- AgentSession (multi-turn, project #183) ---

    #[tokio::test]
    async fn session_prompt_returns_assistant_text_and_history() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(MockProvider::new(vec![end_turn_resp()]));
        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
        let turn = sess.prompt("hi").await;
        assert_eq!(turn.text, "done");
        assert_eq!(turn.stop_reason, StopReason::EndTurn);
        assert_eq!(sess.history().len(), 2); // user + assistant
    }

    #[tokio::test]
    async fn session_prompt_message_preserves_multimodal_user_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let provider = Box::new(CaptureProvider {
            seen: Arc::clone(&seen),
        });
        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
        let message = Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text("describe".into()),
                ContentBlock::Image {
                    data: "aW1hZ2U=".into(),
                    media_type: "image/png".into(),
                    uri: None,
                },
            ],
        };

        sess.prompt_message(message).await;

        assert!(matches!(
            &seen.lock().unwrap()[0].content[1],
            ContentBlock::Image {
                data,
                media_type,
                ..
            } if data == "aW1hZ2U=" && media_type == "image/png"
        ));
        assert!(matches!(
            &sess.history()[0].content[1],
            ContentBlock::Image { .. }
        ));
    }

    #[tokio::test]
    async fn session_accumulates_history_and_usage_across_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(MockProvider::new(vec![end_turn_resp(), end_turn_resp()]));
        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
        sess.prompt("first").await;
        assert_eq!(sess.history().len(), 2);
        sess.prompt("second").await;
        assert_eq!(
            sess.history().len(),
            4,
            "history must persist across prompts"
        );
        // each end_turn_resp reports mock_usage(100, 50)
        assert_eq!(sess.total_usage().input, 200);
        assert_eq!(sess.total_usage().output, 100);
    }

    #[tokio::test]
    async fn session_tool_call_turn_roundtrips_then_finishes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "hello").unwrap();
        let provider = Box::new(MockProvider::new(vec![
            tool_call_resp("c1", "read_file", json!({"path": "f.txt"})),
            end_turn_resp(),
        ]));
        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
        let turn = sess.prompt("read f.txt").await;
        assert_eq!(turn.stop_reason, StopReason::EndTurn);
        assert_eq!(turn.text, "done");
        // user, assistant(toolcall), user(toolresult), assistant(end) = 4
        assert_eq!(sess.history().len(), 4);
        assert!(matches!(
            sess.history()[1].content[0],
            ContentBlock::ToolCall { .. }
        ));
        assert!(matches!(
            sess.history()[2].content[0],
            ContentBlock::ToolResult { .. }
        ));
        assert_eq!(
            sess.config
                .generation_ordinal
                .load(std::sync::atomic::Ordering::Relaxed),
            2,
            "tool loop must assign one ordinal per provider generation"
        );
    }

    #[tokio::test]
    async fn execute_script_runs_in_the_agent_loop() {
        // vikunja #1050 phase (a): execute_script is now dispatchable inside
        // agent::run (previously it fell through to "not available in agent
        // mode"). Its Starlark sandbox shares the loop's session.
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(MockProvider::new(vec![
            tool_call_resp("c1", "execute_script", json!({"code": "result = 40 + 2"})),
            end_turn_resp(),
        ]));
        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
        let turn = sess.prompt("compute").await;
        assert_eq!(turn.stop_reason, StopReason::EndTurn);
        assert_eq!(sess.history().len(), 4);
        match &sess.history()[2].content[0] {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                assert!(
                    !is_error,
                    "execute_script errored in the agent loop: {content}"
                );
                assert!(
                    content.contains("42"),
                    "expected the script result 42, got: {content}"
                );
            }
            other => panic!("expected a tool result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn remote_tool_dispatch_serves_unknown_tool() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(MockProvider::new(vec![
            tool_call_resp("c1", "mcp__srv__echo", json!({"msg": "hi"})),
            end_turn_resp(),
        ]));
        let config = AgentConfig {
            remote_tool_dispatch: Some(Box::new(|name: &str, input: &Value| {
                let name = name.to_string();
                let input = input.clone();
                Box::pin(async move {
                    (name == "mcp__srv__echo").then(|| RemoteToolResult {
                        content: format!("echoed:{}", input["msg"].as_str().unwrap_or("")),
                        is_error: false,
                    })
                })
            })),
            ..AgentConfig::default()
        };
        let mut sess = AgentSession::new(provider, session_in(dir.path()), config);
        let turn = sess.prompt("go").await;
        assert_eq!(turn.stop_reason, StopReason::EndTurn);
        match &sess.history()[2].content[0] {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                assert_eq!(content, "echoed:hi");
                assert!(!is_error);
            }
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    // Wiring-level `tool.call` / `mcp.remote_tool` span emission from
    // `agent::run` and the bridge is validated by the deterministic
    // observability primitive tests (`observability::tests::*`): capturing
    // spans through the real async loop under parallel `cargo test` is flaky
    // because `tracing`'s process-global callsite/level state is raced by
    // other suites' filtering subscribers. The primitives cover every span
    // shape (native/blocked/error/timeout/remote/unavailable, nesting, and
    // the metadata-only privacy contract) without that fragility.

    #[test]
    fn plan_input_normalizes_and_validates_entries() {
        let entries = parse_plan_entries(&json!({
            "entries": [{
                "content": "  implement feature  ",
                "priority": "high",
                "status": "in_progress"
            }]
        }))
        .unwrap();
        assert_eq!(
            entries,
            vec![PlanEntry {
                content: "implement feature".to_string(),
                priority: PlanPriority::High,
                status: PlanStatus::InProgress,
            }]
        );
        assert!(parse_plan_entries(&json!({
            "entries": [{"content": " ", "priority": "low", "status": "pending"}]
        }))
        .unwrap_err()
        .contains("content must not be empty"));
        assert!(parse_plan_entries(&json!({"entries": [], "extra": true})).is_ok());
    }

    #[tokio::test]
    async fn update_plan_invokes_hook_and_records_successful_tool_result() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(MockProvider::new(vec![
            tool_call_resp(
                "plan-1",
                UPDATE_PLAN_TOOL,
                json!({
                    "entries": [
                        {"content": "inspect", "priority": "high", "status": "completed"},
                        {"content": "implement", "priority": "medium", "status": "in_progress"}
                    ]
                }),
            ),
            end_turn_resp(),
        ]));
        let plans: Arc<std::sync::Mutex<Vec<Vec<PlanEntry>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let plans_for_hook = Arc::clone(&plans);
        let config = AgentConfig {
            on_plan_update: Some(Box::new(move |entries| {
                plans_for_hook.lock().unwrap().push(entries.to_vec());
            })),
            ..AgentConfig::default()
        };
        let mut session = AgentSession::new(provider, session_in(dir.path()), config);

        let turn = session.prompt("do it").await;

        assert_eq!(turn.stop_reason, StopReason::EndTurn);
        let plans = plans.lock().unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0][1].content, "implement");
        assert_eq!(plans[0][1].status, PlanStatus::InProgress);
        match &session.history()[2].content[0] {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                assert!(!is_error);
                assert_eq!(content, "{\"updated\":2}");
            }
            other => panic!("expected plan tool result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn remote_tool_dispatch_none_falls_through_to_not_available() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(MockProvider::new(vec![
            tool_call_resp("c1", "totally_unknown", json!({})),
            end_turn_resp(),
        ]));
        // Hook declines (returns None) → the loop's "not available" fallback applies.
        let config = AgentConfig {
            remote_tool_dispatch: Some(Box::new(|_name: &str, _input: &Value| {
                Box::pin(async move { None })
            })),
            ..AgentConfig::default()
        };
        let mut sess = AgentSession::new(provider, session_in(dir.path()), config);
        sess.prompt("go").await;
        match &sess.history()[2].content[0] {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                assert!(content.contains("not available"));
                assert!(is_error);
            }
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_clear_resets_history_keeps_usage() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(MockProvider::new(vec![end_turn_resp()]));
        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
        sess.prompt("hi").await;
        assert_eq!(sess.history().len(), 2);
        sess.clear();
        assert_eq!(sess.history().len(), 0);
        assert_eq!(
            sess.total_usage().input,
            100,
            "cumulative usage kept after clear"
        );
    }

    #[tokio::test]
    async fn session_set_model_changes_model_sent_on_next_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let config = AgentConfig {
            opts: CompleteOpts {
                model: "model-a".to_string(),
                ..CompleteOpts::default()
            },
            ..AgentConfig::default()
        };
        // Capture the model the provider actually sees each turn.
        struct ModelCapture(std::sync::Arc<Mutex<Vec<String>>>);
        #[async_trait]
        impl LlmProvider for ModelCapture {
            async fn complete(&self, _ctx: &Context, opts: &CompleteOpts) -> LlmResponse {
                self.0.lock().unwrap().push(opts.model.clone());
                end_turn_resp()
            }
        }
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let provider = Box::new(ModelCapture(std::sync::Arc::clone(&seen)));
        let mut sess = AgentSession::new(provider, session_in(dir.path()), config);
        assert_eq!(sess.model(), "model-a");

        sess.prompt("first").await;
        sess.set_model("model-b");
        assert_eq!(sess.model(), "model-b");
        sess.prompt("second").await;

        assert_eq!(
            *seen.lock().unwrap(),
            vec!["model-a".to_string(), "model-b".to_string()]
        );
        // History persists across the model switch (user + asst) x2.
        assert_eq!(sess.history().len(), 4);
    }

    #[tokio::test]
    async fn retry_replaces_latest_turn_without_duplicate_user_message() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(MockProvider::new(vec![
            end_turn_resp_with_text("first"),
            end_turn_resp_with_text("second"),
            end_turn_resp_with_text("second retried"),
        ]));
        let mut session =
            AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
        session.prompt("one").await;
        session.prompt("two").await;

        let retried = session.retry_last_turn().await.unwrap();

        assert_eq!(retried.text, "second retried");
        assert_eq!(session.user_turn_count(), 2);
        assert_eq!(session.history().len(), 4);
        assert!(matches!(
            &session.history()[2].content[0],
            ContentBlock::Text(text) if text == "two"
        ));
    }

    #[tokio::test]
    async fn truncate_removes_selected_user_turn_and_everything_after() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(MockProvider::new(vec![
            end_turn_resp_with_text("first"),
            end_turn_resp_with_text("second"),
        ]));
        let mut session =
            AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
        session.prompt("one").await;
        session.prompt("two").await;

        session.truncate_from_user_turn(1).unwrap();

        assert_eq!(session.user_turn_count(), 1);
        assert_eq!(session.history().len(), 2);
        assert!(session.truncate_from_user_turn(1).is_err());
    }

    // --- token_log (vikunja: --debug-tokens) ---

    #[test]
    fn token_log_line_has_expected_fields() {
        let usage = Usage {
            input: 120,
            output: 45,
            cache_read: 3,
            cache_write: 7,
            cost: Cost {
                total_usd: 0.0012,
                ..Cost::default()
            },
        };
        let line = token_log_line("chat", "claude-haiku-4-5", &usage);
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["cmd"], "chat");
        assert_eq!(parsed["model"], "claude-haiku-4-5");
        assert_eq!(parsed["input"], 120);
        assert_eq!(parsed["output"], 45);
        assert_eq!(parsed["cache_read"], 3);
        assert_eq!(parsed["cache_write"], 7);
        assert_eq!(parsed["cost_usd"], "0.001200");
        assert!(parsed["ts"].is_string(), "must include a timestamp");
    }

    #[test]
    fn log_token_usage_appends_one_line_per_call() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = TokenLogConfig {
            path: dir.path().join("tokens.log"),
            label: "agent".to_string(),
        };
        log_token_usage(&cfg, "m1", &mock_usage(10, 5));
        log_token_usage(&cfg, "m1", &mock_usage(20, 8));
        let content = std::fs::read_to_string(&cfg.path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "each call should append exactly one line");
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(first["input"], 10);
        assert_eq!(second["input"], 20);
    }

    #[test]
    fn log_token_usage_creates_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = TokenLogConfig {
            path: dir.path().join("nested_does_not_exist_yet.log"),
            label: "agent".to_string(),
        };
        log_token_usage(&cfg, "m1", &mock_usage(1, 1));
        assert!(cfg.path.exists());
    }

    #[tokio::test]
    async fn run_writes_token_log_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("tokens.log");
        let s = session_in(dir.path());
        let provider = MockProvider::new(vec![end_turn_resp()]);
        let config = AgentConfig {
            token_log: Some(TokenLogConfig {
                path: log_path.clone(),
                label: "agent".to_string(),
            }),
            ..AgentConfig::default()
        };
        run(&provider, shared(s), vec![Message::user("hi")], &config).await;
        let content = std::fs::read_to_string(&log_path).unwrap();
        assert_eq!(content.lines().count(), 1);
        assert!(content.contains("\"cmd\":\"agent\""));
    }

    #[tokio::test]
    async fn run_does_not_write_token_log_when_not_configured() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("tokens.log");
        let s = session_in(dir.path());
        let provider = MockProvider::new(vec![end_turn_resp()]);
        run(
            &provider,
            shared(s),
            vec![Message::user("hi")],
            &AgentConfig::default(),
        )
        .await;
        assert!(!log_path.exists());
    }

    // --- accumulate_usage ---

    #[test]
    fn accumulate_sums_tokens() {
        let total = accumulate_usage(mock_usage(100, 50), mock_usage(200, 75));
        assert_eq!(total.input, 300);
        assert_eq!(total.output, 125);
    }

    #[test]
    fn accumulate_sums_cost() {
        let a = Usage {
            cost: Cost {
                input_usd: 1.0,
                total_usd: 1.5,
                ..Cost::default()
            },
            ..Usage::default()
        };
        let b = Usage {
            cost: Cost {
                input_usd: 2.0,
                total_usd: 3.0,
                ..Cost::default()
            },
            ..Usage::default()
        };
        let total = accumulate_usage(a, b);
        assert!((total.cost.input_usd - 3.0).abs() < 1e-9);
        assert!((total.cost.total_usd - 4.5).abs() < 1e-9);
    }

    #[test]
    fn accumulate_zero_is_identity() {
        let total = accumulate_usage(mock_usage(500, 250), Usage::default());
        assert_eq!(total.input, 500);
        assert_eq!(total.output, 250);
    }

    // --- response_to_content ---

    #[test]
    fn content_uses_data_field() {
        let resp = Response::ok(json!({"content": "hello"}));
        assert!(response_to_content(resp).contains("hello"));
    }

    #[test]
    fn content_falls_back_to_message() {
        let resp = Response::err(3, "tool failed");
        assert_eq!(response_to_content(resp), "tool failed");
    }

    #[test]
    fn remote_tools_are_appended_to_list_all_tools_catalog() {
        let native = serde_json::json!([
            {"name": "read_file", "description": "Read a file"}
        ])
        .to_string();
        let tools = vec![
            ToolSchema {
                name: "read_file".to_string(),
                description: "Read a file".to_string(),
                input_schema: json!({"type": "object"}),
            },
            ToolSchema {
                name: "mcp__linear__get_issue".to_string(),
                description: "Get a Linear issue".to_string(),
                input_schema: json!({"type": "object"}),
            },
        ];

        let catalog = append_remote_tools_to_catalog(native, &tools);
        let entries: Vec<Value> = serde_json::from_str(&catalog).unwrap();

        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry["name"] == "read_file")
                .count(),
            1
        );
        assert!(entries
            .iter()
            .any(|entry| entry["name"] == "mcp__linear__get_issue"));
    }

    // --- streaming (vikunja #957) ---

    struct StreamingMockProvider {
        events: Vec<StreamEvent>,
        response: LlmResponse,
    }

    #[async_trait]
    impl LlmProvider for StreamingMockProvider {
        async fn complete(&self, _ctx: &Context, _opts: &CompleteOpts) -> LlmResponse {
            panic!("StreamingMockProvider expects stream(), not complete()");
        }

        async fn stream(
            &self,
            _ctx: &Context,
            _opts: &CompleteOpts,
            on_event: &mut (dyn FnMut(StreamEvent) + Send),
        ) -> LlmResponse {
            for ev in self.events.clone() {
                on_event(ev);
            }
            LlmResponse {
                content: self.response.content.clone(),
                stop_reason: self.response.stop_reason.clone(),
                error_message: self.response.error_message.clone(),
                context_overflow: false,
                usage: self.response.usage.clone(),
            }
        }
    }

    #[tokio::test]
    async fn run_forwards_stream_events_to_hook() {
        // The `on_stream_event` hook must receive each streamed delta in order.
        // (Time-to-first-token span recording is covered by a synchronous unit
        // test in `observability.rs` — asserting it here through the async
        // run + a tracing subscriber was a pre-existing flake under parallel
        // `cargo test`.)
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        let provider = StreamingMockProvider {
            events: vec![
                StreamEvent::TextDelta("hel".into()),
                StreamEvent::TextDelta("lo".into()),
            ],
            response: end_turn_resp(),
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = Arc::clone(&seen);
        let config = AgentConfig {
            on_stream_event: Some(Box::new(move |ev| seen_clone.lock().unwrap().push(ev))),
            ..AgentConfig::default()
        };
        let result = run(&provider, shared(s), vec![Message::user("hi")], &config).await;
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        let seen = seen.lock().unwrap();
        assert_eq!(
            *seen,
            vec![
                StreamEvent::TextDelta("hel".into()),
                StreamEvent::TextDelta("lo".into())
            ]
        );
    }

    #[tokio::test]
    async fn run_without_hook_still_calls_stream_and_ignores_events() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        let provider = StreamingMockProvider {
            events: vec![StreamEvent::TextDelta("x".into())],
            response: end_turn_resp(),
        };
        let result = run(
            &provider,
            shared(s),
            vec![Message::user("hi")],
            &AgentConfig::default(),
        )
        .await;
        assert_eq!(result.stop_reason, StopReason::EndTurn);
    }

    // --- run loop ---

    #[tokio::test]
    async fn end_turn_stops_loop() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        let provider = MockProvider::new(vec![end_turn_resp()]);
        let result = run(
            &provider,
            shared(s),
            vec![Message::user("hi")],
            &AgentConfig::default(),
        )
        .await;
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert!(result.error_message.is_none());
        assert_eq!(result.messages.len(), 2); // user + assistant
    }

    #[tokio::test]
    async fn provider_error_stops_loop_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        let provider = MockProvider::new(vec![LlmResponse::error("API failed")]);
        let result = run(
            &provider,
            shared(s),
            vec![Message::user("hi")],
            &AgentConfig::default(),
        )
        .await;
        assert_eq!(result.stop_reason, StopReason::Error);
        assert_eq!(result.error_message.as_deref(), Some("API failed"));
    }

    #[tokio::test]
    async fn max_tokens_stops_loop_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        let provider = MockProvider::new(vec![LlmResponse {
            content: vec![],
            stop_reason: StopReason::MaxTokens,
            error_message: None,
            context_overflow: false,
            usage: Usage::default(),
        }]);
        let result = run(
            &provider,
            shared(s),
            vec![Message::user("go")],
            &AgentConfig::default(),
        )
        .await;
        assert_eq!(result.stop_reason, StopReason::MaxTokens);
    }

    #[tokio::test]
    async fn usage_accumulates_across_turns() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        let provider = MockProvider::new(vec![
            tool_call_resp("t1", "nonexistent_tool", json!({})),
            LlmResponse {
                content: vec![ContentBlock::Text("done".into())],
                stop_reason: StopReason::EndTurn,
                error_message: None,
                context_overflow: false,
                usage: mock_usage(300, 150),
            },
        ]);
        let result = run(
            &provider,
            shared(s),
            vec![Message::user("go")],
            &AgentConfig::default(),
        )
        .await;
        assert_eq!(result.usage.input, 500); // 200 + 300
        assert_eq!(result.usage.output, 250); // 100 + 150
    }

    #[tokio::test]
    async fn assistant_appended_before_tool_results() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        let provider = MockProvider::new(vec![
            tool_call_resp("t1", "nonexistent_tool", json!({})),
            end_turn_resp(),
        ]);
        let result = run(
            &provider,
            shared(s),
            vec![Message::user("go")],
            &AgentConfig::default(),
        )
        .await;
        // user, assistant(tool_call), user(tool_result), assistant(end_turn)
        assert_eq!(result.messages.len(), 4);
        assert_eq!(result.messages[1].role, Role::Assistant);
        assert!(matches!(
            &result.messages[1].content[0],
            ContentBlock::ToolCall { .. }
        ));
        assert_eq!(result.messages[2].role, Role::User);
        assert!(matches!(
            &result.messages[2].content[0],
            ContentBlock::ToolResult { .. }
        ));
    }

    #[tokio::test]
    async fn unknown_tool_becomes_is_error_result() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        let provider = MockProvider::new(vec![
            tool_call_resp("t1", "does_not_exist", json!({})),
            end_turn_resp(),
        ]);
        let result = run(
            &provider,
            shared(s),
            vec![Message::user("go")],
            &AgentConfig::default(),
        )
        .await;
        assert!(matches!(
            &result.messages[2].content[0],
            ContentBlock::ToolResult { is_error: true, .. }
        ));
    }

    #[tokio::test]
    async fn real_tool_call_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.txt"), "hello agent").unwrap();
        let s = session_in(dir.path());
        let provider = MockProvider::new(vec![
            tool_call_resp("t1", "read_file", json!({"path": "test.txt"})),
            end_turn_resp(),
        ]);
        let result = run(
            &provider,
            shared(s),
            vec![Message::user("read it")],
            &AgentConfig::default(),
        )
        .await;
        if let ContentBlock::ToolResult {
            content, is_error, ..
        } = &result.messages[2].content[0]
        {
            assert!(!is_error, "real tool should succeed");
            assert!(content.contains("hello agent"));
        } else {
            panic!("expected ToolResult");
        }
    }

    #[tokio::test]
    async fn before_hook_block_returns_error_result() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        let provider = MockProvider::new(vec![
            tool_call_resp("t1", "exec", json!({"command": "rm -rf /"})),
            end_turn_resp(),
        ]);
        let config = AgentConfig {
            before_tool_call: Some(Box::new(|_| {
                Box::pin(std::future::ready(BeforeHookResult::Block(
                    "not permitted".into(),
                )))
            })),
            ..AgentConfig::default()
        };
        let result = run(&provider, shared(s), vec![Message::user("go")], &config).await;
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert!(matches!(
            &result.messages[2].content[0],
            ContentBlock::ToolResult { is_error: true, content, .. } if content.contains("blocked")
        ));
    }

    #[tokio::test]
    async fn tool_progress_hook_receives_exec_output_and_exit() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_in(dir.path());
        let provider = MockProvider::new(vec![
            tool_call_resp("t1", "exec", json!({"command": "printf streamed-output"})),
            end_turn_resp(),
        ]);
        let events = Arc::new(Mutex::new(Vec::new()));
        let events_for_hook = Arc::clone(&events);
        let config = AgentConfig {
            on_tool_progress: Some(Box::new(move |info, event| {
                events_for_hook
                    .lock()
                    .unwrap()
                    .push((info.id.clone(), event));
            })),
            ..AgentConfig::default()
        };

        run(
            &provider,
            shared(session),
            vec![Message::user("run command")],
            &config,
        )
        .await;

        let events = events.lock().unwrap();
        assert!(events.iter().any(
            |(id, event)| id == "t1"
                && matches!(event, crate::ops::ExecProgress::Output(data) if data.contains("streamed-output"))
        ));
        assert!(matches!(
            events.last(),
            Some((
                id,
                crate::ops::ExecProgress::Exit {
                    code: Some(0),
                    ..
                }
            )) if id == "t1"
        ));
    }

    #[tokio::test]
    async fn after_hook_terminate_exits_with_aborted() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        let provider = MockProvider::new(vec![
            tool_call_resp("t1", "nonexistent_tool", json!({})),
            end_turn_resp(), // should not be reached
        ]);
        let config = AgentConfig {
            after_tool_call: Some(Box::new(|_, _, _| AfterHookResult::Terminate)),
            ..AgentConfig::default()
        };
        let result = run(&provider, shared(s), vec![Message::user("go")], &config).await;
        assert_eq!(result.stop_reason, StopReason::Aborted);
        assert!(result
            .error_message
            .as_deref()
            .unwrap_or("")
            .contains("terminated"));
    }

    #[tokio::test]
    async fn thinking_blocks_retained_in_assistant_turn() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        let provider = MockProvider::new(vec![LlmResponse {
            content: vec![
                ContentBlock::Thinking("my reasoning".into()),
                ContentBlock::Text("my answer".into()),
            ],
            stop_reason: StopReason::EndTurn,
            error_message: None,
            context_overflow: false,
            usage: Usage::default(),
        }]);
        let result = run(
            &provider,
            shared(s),
            vec![Message::user("think")],
            &AgentConfig::default(),
        )
        .await;
        let assistant = &result.messages[1];
        assert_eq!(assistant.content.len(), 2);
        assert!(matches!(&assistant.content[0], ContentBlock::Thinking(t) if t == "my reasoning"));
    }

    // --- context compaction (ADR-002, vikunja #962) ---

    /// One recorded API call: the model, temperature, and system prompt the
    /// session sent — the compaction tests assert the summarization call's
    /// shape with it.
    type RecordedCall = (String, Option<f64>, Option<String>);

    /// Scripted responses plus per-call recording.
    struct RecordingProvider {
        responses: std::sync::Mutex<VecDeque<LlmResponse>>,
        calls: std::sync::Arc<std::sync::Mutex<Vec<RecordedCall>>>,
    }

    impl RecordingProvider {
        fn new(responses: Vec<LlmResponse>) -> Self {
            RecordingProvider {
                responses: std::sync::Mutex::new(VecDeque::from(responses)),
                calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }

        /// A shared handle to the recorded calls that stays valid after the
        /// provider is moved into `AgentSession::new` (which relocates it into
        /// an `Arc`, so a raw pointer to the boxed provider would dangle).
        fn calls_handle(&self) -> std::sync::Arc<std::sync::Mutex<Vec<RecordedCall>>> {
            std::sync::Arc::clone(&self.calls)
        }
    }

    #[async_trait]
    impl LlmProvider for RecordingProvider {
        async fn complete(&self, ctx: &Context, opts: &CompleteOpts) -> LlmResponse {
            self.calls.lock().unwrap().push((
                opts.model.clone(),
                opts.temperature,
                ctx.system.clone(),
            ));
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| LlmResponse::error("RecordingProvider exhausted"))
        }
    }

    /// budget = 800, trigger ≥ 600 measured tokens, evict down to ~400
    /// estimated tokens.
    fn test_policy() -> CompactionPolicy {
        CompactionPolicy {
            high_water: 0.75,
            low_water: 0.5,
            context_window: 1000,
            output_reservation: 200,
            summary_model: None,
            summary_prompt: None,
        }
    }

    fn compaction_config(policy: CompactionPolicy) -> AgentConfig {
        AgentConfig {
            compaction: Some(policy),
            ..AgentConfig::default()
        }
    }

    fn end_turn_with_usage(text: &str, input: u64) -> LlmResponse {
        LlmResponse {
            content: vec![ContentBlock::Text(text.to_string())],
            stop_reason: StopReason::EndTurn,
            error_message: None,
            context_overflow: false,
            usage: mock_usage(input, 10),
        }
    }

    /// A user prompt big enough (~1000 estimated tokens) that the chars/4
    /// cut-sizing sees real weight per turn.
    fn big_text(tag: &str) -> String {
        format!("{tag} {}", "x".repeat(4000))
    }

    #[tokio::test]
    async fn proactive_compaction_fires_over_high_water() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(RecordingProvider::new(vec![
            end_turn_with_usage("a1", 100),         // turn 1: under trigger
            end_turn_with_usage("a2", 700),         // turn 2: measured 700 ≥ 600 → arms trigger
            end_turn_with_usage("the summary", 50), // summarization call
            end_turn_with_usage("a3", 100),         // turn 3 runs on compacted history
        ]));
        let mut sess = AgentSession::new(
            provider,
            session_in(dir.path()),
            compaction_config(test_policy()),
        );

        sess.prompt(big_text("t1")).await;
        sess.prompt(big_text("t2")).await;
        let turn = sess.prompt("t3").await;
        assert_eq!(turn.text, "a3");

        // Turn 1 evicted, replaced by the summary; turns 2–3 kept verbatim.
        let history = sess.history();
        assert!(
            matches!(&history[0].content[0], ContentBlock::Text(t) if t.contains("[Summary of earlier conversation: the summary]")),
            "history[0] must be the summary message: {history:?}"
        );
        assert!(
            !history.iter().any(|m| m
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text(t) if t.contains("t1 ")))),
            "turn 1 must be evicted"
        );
        assert!(
            history.iter().any(|m| m
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text(t) if t.contains("t2 ")))),
            "turn 2 must be kept verbatim"
        );
        assert_eq!(
            history.len(),
            5,
            "summary + turn2 (2 msgs) + turn3 (2 msgs): {history:?}"
        );
    }

    #[tokio::test]
    async fn compaction_disabled_never_compacts() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(RecordingProvider::new(vec![
            end_turn_with_usage("a1", 100),
            end_turn_with_usage("a2", 700),
            end_turn_with_usage("a3", 100),
        ]));
        let calls_handle = provider.calls_handle();
        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
        sess.prompt(big_text("t1")).await;
        sess.prompt(big_text("t2")).await;
        sess.prompt("t3").await;
        let calls = calls_handle.lock().unwrap().clone();
        assert_eq!(calls.len(), 3, "no summarization call must be made");
        assert_eq!(sess.history().len(), 6, "nothing evicted");
    }

    #[tokio::test]
    async fn reactive_overflow_compacts_and_retries_once() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(RecordingProvider::new(vec![
            end_turn_with_usage("a1", 100),
            end_turn_with_usage("a2", 100), // stays under the proactive trigger
            LlmResponse::context_overflow_error("prompt is too long"),
            end_turn_with_usage("the summary", 50),
            end_turn_with_usage("a3 after retry", 100),
        ]));
        let mut sess = AgentSession::new(
            provider,
            session_in(dir.path()),
            compaction_config(test_policy()),
        );

        sess.prompt(big_text("t1")).await;
        sess.prompt(big_text("t2")).await;
        let turn = sess.prompt("t3").await;

        assert_eq!(
            turn.text, "a3 after retry",
            "the retried turn's answer must come back"
        );
        assert!(
            turn.error_message.is_none(),
            "overflow must be recovered, not surfaced"
        );
        let history = sess.history();
        assert!(
            matches!(&history[0].content[0], ContentBlock::Text(t) if t.contains("[Summary of earlier conversation")),
            "reactive path must have compacted: {history:?}"
        );
        // The failed attempt must not have committed its empty assistant msg.
        assert!(
            !history
                .iter()
                .any(|m| m.role == Role::Assistant && m.content.is_empty()),
            "failed overflow attempt must not be committed: {history:?}"
        );
    }

    #[tokio::test]
    async fn overflow_without_policy_surfaces_error_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(RecordingProvider::new(vec![
            LlmResponse::context_overflow_error("prompt is too long"),
        ]));
        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
        let turn = sess.prompt("hi").await;
        assert!(
            turn.error_message.is_some(),
            "compaction off → the error surfaces as before"
        );
    }

    #[tokio::test]
    async fn summary_call_uses_summary_model_and_temperature_zero() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(RecordingProvider::new(vec![
            end_turn_with_usage("a1", 100),
            end_turn_with_usage("a2", 700),
            end_turn_with_usage("the summary", 50),
            end_turn_with_usage("a3", 100),
        ]));
        let calls_handle = provider.calls_handle();
        let policy = CompactionPolicy {
            summary_model: Some("cheap-summarizer".to_string()),
            summary_prompt: Some("Custom summary instructions.".to_string()),
            ..test_policy()
        };
        let mut sess =
            AgentSession::new(provider, session_in(dir.path()), compaction_config(policy));
        sess.prompt(big_text("t1")).await;
        sess.prompt(big_text("t2")).await;
        sess.prompt("t3").await;

        let calls = calls_handle.lock().unwrap().clone();
        assert_eq!(calls.len(), 4);
        let (model, temperature, system) = &calls[2]; // the summarization call
        assert_eq!(model, "cheap-summarizer");
        assert_eq!(*temperature, Some(0.0), "summary must be deterministic");
        assert_eq!(system.as_deref(), Some("Custom summary instructions."));
        // Ordinary turn calls keep the session model and no temperature.
        assert_eq!(calls[3].0, CompleteOpts::default().model);
        assert_eq!(calls[3].1, None);
    }

    #[tokio::test]
    async fn summarizer_failure_falls_back_to_drop_marker() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(RecordingProvider::new(vec![
            end_turn_with_usage("a1", 100),
            end_turn_with_usage("a2", 700),
            LlmResponse::error("summarizer down"), // attempt
            LlmResponse::error("still down"),      // retry
            end_turn_with_usage("a3", 100),
        ]));
        let mut sess = AgentSession::new(
            provider,
            session_in(dir.path()),
            compaction_config(test_policy()),
        );
        sess.prompt(big_text("t1")).await;
        sess.prompt(big_text("t2")).await;
        let turn = sess.prompt("t3").await;

        assert_eq!(
            turn.text, "a3",
            "the session must keep working after the fallback"
        );
        assert!(
            matches!(&sess.history()[0].content[0], ContentBlock::Text(t) if t.contains("summary unavailable")),
            "drop marker must replace the evicted turns: {:?}",
            sess.history()
        );
    }

    #[tokio::test]
    async fn compaction_hook_and_token_log_report_the_event() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("tokens.log");
        let events: Arc<std::sync::Mutex<Vec<(usize, bool)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_in_hook = Arc::clone(&events);

        let provider = Box::new(RecordingProvider::new(vec![
            end_turn_with_usage("a1", 100),
            end_turn_with_usage("a2", 700),
            end_turn_with_usage("the summary", 50),
            end_turn_with_usage("a3", 100),
        ]));
        let config = AgentConfig {
            compaction: Some(test_policy()),
            on_compaction: Some(Box::new(move |event| {
                events_in_hook
                    .lock()
                    .unwrap()
                    .push((event.evicted_turns, event.fallback_drop));
            })),
            token_log: Some(TokenLogConfig {
                path: log_path.clone(),
                label: "test".to_string(),
            }),
            ..AgentConfig::default()
        };
        let mut sess = AgentSession::new(provider, session_in(dir.path()), config);
        sess.prompt(big_text("t1")).await;
        sess.prompt(big_text("t2")).await;
        sess.prompt("t3").await;

        let seen = events.lock().unwrap();
        assert_eq!(seen.len(), 1, "exactly one compaction");
        assert_eq!(seen[0], (1, false), "one turn evicted, no fallback");

        let log = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            log.contains("\"event\":\"compaction\""),
            "structured event line expected: {log}"
        );
        assert!(log.contains("\"strategy\":\"summarize\""), "{log}");
    }

    #[tokio::test]
    async fn summary_usage_counts_toward_session_total() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(RecordingProvider::new(vec![
            end_turn_with_usage("a1", 100),         // +100 input
            end_turn_with_usage("a2", 700),         // +700
            end_turn_with_usage("the summary", 40), // +40 (summarization call)
            end_turn_with_usage("a3", 100),         // +100
        ]));
        let mut sess = AgentSession::new(
            provider,
            session_in(dir.path()),
            compaction_config(test_policy()),
        );
        sess.prompt(big_text("t1")).await;
        sess.prompt(big_text("t2")).await;
        sess.prompt("t3").await;
        assert_eq!(
            sess.total_usage().input,
            940,
            "summary call tokens must be counted"
        );
    }
}
