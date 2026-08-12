#![allow(dead_code)]

use std::collections::HashSet;
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

struct GenerationLogMetadata<'a> {
    kind: &'a str,
    ordinal: u64,
    stop_reason: &'a StopReason,
    response_tool_calls: usize,
    context: &'a crate::context_metrics::ContextComposition,
    /// Tool ops dispatched inside `execute_script` so far this run, summed
    /// across every script. Run state, not context state — deliberately not on
    /// `ContextComposition`, which is contractually derived from `Context`
    /// alone (vikunja #1230).
    script_ops_total: usize,
    /// Ops dispatched by the *largest single* script so far. This is the batch
    /// adoption discriminator: see [`is_batch_adoption`].
    script_ops_max: usize,
}

/// Did this run batch? Measured by ops collapsed into one round-trip, not by
/// script size (vikunja #1230).
///
/// A 1-op script is **not** adoption — it is a single tool call in a costume,
/// which is what forcing `execute_script` produced in the B1 arm (a 71-byte
/// inspection script) and what a byte threshold cannot distinguish from real
/// multi-op work. Two or more ops in one script means N round-trips became 1,
/// which is the cost lever this task exists to move.
///
/// Deliberately not a size test: the verified-optimal solution to bench task 03
/// serializes to ~250-550 bytes and would score zero against the old 700-byte
/// threshold, while a 1701-byte forced script that errored twice would score as
/// adoption. Size rewards verbosity; op count measures the lever.
pub fn is_batch_adoption(script_ops_max: usize) -> bool {
    script_ops_max >= 2
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
    /// Automatic continuations after a `max_tokens` stop within a single user
    /// turn (item 3 of the large-output UX work). `None` falls back to the
    /// `DAIMONOS_AGENT_AUTO_CONTINUE` env var, then to
    /// [`DEFAULT_AUTO_CONTINUE_BUDGET`]. `Some(0)` disables it, so the turn
    /// returns a terminal `MaxTokens` exactly as before. Each auto-continuation
    /// reissues the truncated turn with thinking forced off so reasoning cannot
    /// re-consume the output budget and stall progress.
    pub auto_continue_budget: Option<u32>,
    /// Bounded retry for *transient* provider failures (vikunja #1240).
    pub provider_retry: ProviderRetryConfig,
}

/// How hard to retry a provider failure the adapter classified as transient
/// (`LlmResponse::retryable`), before surfacing it to the caller.
///
/// Only transient failures are retried; see [`crate::providers::is_retryable_status`]
/// for what qualifies. Retrying a fatal error would mask a real
/// misconfiguration behind latency and burned tokens.
#[derive(Debug, Clone)]
pub struct ProviderRetryConfig {
    /// Extra attempts *after* the first. `0` disables retry entirely, restoring
    /// the pre-#1240 behaviour of surfacing the first transient error.
    pub max_attempts: u32,
    /// First backoff interval; doubles per attempt (see [`retry_backoff`]).
    /// `Duration::ZERO` makes retries immediate, which is what tests want.
    pub base_delay: std::time::Duration,
    /// Ordered failover chain. When `max_attempts` same-model retries are
    /// exhausted and the failure is still transient, the loop advances to the
    /// next entry *after the active model* and retries there with a fresh
    /// budget.
    ///
    /// **Opt-in: empty disables failover entirely.** Populate from
    /// `AgentEnv::models`, which already yields a deduped ordered list with the
    /// active model guaranteed present — so "the next one after the active
    /// model" is always well defined.
    ///
    /// Only transient failures advance the chain. Failing over on a fatal error
    /// would multiply one misconfiguration (bad key, malformed request) across
    /// every model in the list.
    pub failover_models: Vec<String>,
}

impl Default for ProviderRetryConfig {
    fn default() -> Self {
        // Two retries at 500ms/1s covers the overwhelming majority of 429s and
        // 5xx blips without making a genuinely-down provider feel hung: worst
        // case adds ~1.5s before the error surfaces.
        ProviderRetryConfig {
            max_attempts: 2,
            base_delay: std::time::Duration::from_millis(500),
            // Opt-in: no chain means no failover.
            failover_models: Vec::new(),
        }
    }
}

/// The next model to try after `active` in an ordered failover chain, or `None`
/// when `active` is the last entry (or absent from the chain).
///
/// Position-based rather than round-robin: once a model has failed for this
/// generation, cycling back to it would spend the whole budget on a provider
/// already known to be degraded.
fn next_failover_model<'a>(chain: &'a [String], active: &str) -> Option<&'a str> {
    let at = chain.iter().position(|m| m == active)?;
    chain.get(at + 1).map(String::as_str)
}

/// Exponential backoff for retry `attempt` (1-based): `base * 2^(attempt-1)`.
///
/// No jitter: daimonos runs one agent loop per process against a per-user rate
/// limit, so there is no thundering herd to de-correlate — jitter would only
/// make the delay untestable. Revisit if a single process ever fans out
/// concurrent generations against one provider.
fn retry_backoff(base: std::time::Duration, attempt: u32) -> std::time::Duration {
    base.saturating_mul(1u32 << attempt.saturating_sub(1).min(6))
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
        reasoning_output: acc.reasoning_output + turn.reasoning_output,
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

/// Reconcile a `list_all_tools` catalog with what the agent can actually call.
///
/// `list_all_tools` is served from the **MCP** catalog — `tool_definitions`,
/// i.e. every tier that is `defined_for_mcp` — but the agent's own tool list is
/// `exposed_to_agent`. Those sets differ, and reporting the difference tells the
/// model about tools it cannot use (vikunja #1284):
///
/// - `McpOnly` (`batch`) is intercepted in the MCP request handler and has no
///   agent-side implementation, so it fails on every call. This is precisely the
///   defect #1112 fixed for the schema list, arriving through the catalog.
/// - `OnDemand` tools are dispatchable but carry no schema the model can call,
///   and there is no agent-side activation path (see `ToolTier::exposed_to_agent`).
///
/// Remote MCP bridge tools are not in the local registry and are always kept —
/// they are real entries in `config.tools`, which is what makes them callable.
fn append_remote_tools_to_catalog(content: String, tools: &[ToolSchema]) -> String {
    let Ok(Value::Array(entries)) = serde_json::from_str(&content) else {
        eprintln!("agent: list_all_tools returned a non-array catalog; remote tools omitted");
        return content;
    };
    let mut entries: Vec<Value> = entries
        .into_iter()
        .filter(|entry| {
            let Some(name) = entry.get("name").and_then(Value::as_str) else {
                return true;
            };
            match crate::tools::all_tools().iter().find(|t| t.name == name) {
                Some(tool) => tool.tier.exposed_to_agent(),
                // Not a local registry tool (e.g. an already-listed remote
                // tool): the registry has no opinion, so keep it.
                None => true,
            }
        })
        .collect();
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
fn token_log_line(
    label: &str,
    model: &str,
    usage: &Usage,
    metadata: Option<&GenerationLogMetadata<'_>>,
) -> String {
    let mut line = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "cmd": label,
        "model": model,
        "input": usage.input,
        "output": usage.output,
        "reasoning_output": usage.reasoning_output,
        "cache_read": usage.cache_read,
        "cache_write": usage.cache_write,
        // Fixed-decimal string, not a bare f64: serde_json renders small floats
        // in scientific notation (e.g. 3e-6), which is diff-unfriendly and awkward
        // to parse. Six decimals = microdollar precision, enough for per-call cost.
        "cost_usd": format!("{:.6}", usage.cost.total_usd),
    });
    if let Some(metadata) = metadata {
        line["schema_version"] = serde_json::json!(2);
        line["generation_kind"] = serde_json::json!(metadata.kind);
        line["ordinal"] = serde_json::json!(metadata.ordinal);
        line["stop_reason"] = serde_json::json!(metadata.stop_reason.as_str());
        line["response_tool_calls"] = serde_json::json!(metadata.response_tool_calls);
        // Batch-adoption signal (#1230): ops collapsed into a single script,
        // which is the lever. Counts only — no script source, per ADR-006 D6.
        line["script_ops_total"] = serde_json::json!(metadata.script_ops_total);
        line["script_ops_max"] = serde_json::json!(metadata.script_ops_max);
        line["context"] = serde_json::to_value(metadata.context).unwrap_or(serde_json::Value::Null);
    }
    line.to_string()
}

/// Best-effort append of one token-usage line. Never panics or propagates
/// I/O errors — a debug log must not be able to break the agent loop.
fn log_token_usage(
    cfg: &TokenLogConfig,
    model: &str,
    usage: &Usage,
    metadata: Option<&GenerationLogMetadata<'_>>,
) {
    use std::io::Write;
    let line = token_log_line(&cfg.label, model, usage, metadata);
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

/// Prepare unread agent mail at a safe generation boundary. Returns a
/// metadata-only system notice plus the newest-message watermark it represents.
/// This does NOT advance the watermark: the caller acknowledges it only after
/// a provider response successfully consumes the notice. Fail-open: any
/// store/path error yields None and never fails the turn.
async fn coordination_model_notice(
    session: &std::sync::Arc<tokio::sync::Mutex<Session>>,
) -> Option<(String, i64)> {
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
    // Re-check the binding/watermark snapshot before returning a candidate,
    // but do not advance yet — a provider error/abort must leave it retryable.
    let s = session.lock().await;
    if s.coordination_agent_name.as_deref() != Some(agent.as_str())
        || s.coordination_model_watermark != watermark
    {
        return None;
    }
    Some((
        format!(
            "[DAIMONOS COORDINATION NOTICE]\nYou have {} new unread agent-mail message(s) for {}. Highest importance: {}. Call fetch_inbox(agent=\"{}\", unread_only=true) before continuing if the messages may affect your current work.",
            summary.count, agent, summary.highest_importance, agent
        ),
        summary.newest_message_id,
    ))
}

/// Commit a prepared model notice after the provider successfully consumed it.
/// Monotonic max keeps concurrent/newer candidates safe; an error/abort caller
/// simply does not call this, so the notice is retried on the next turn.
async fn acknowledge_coordination_model_notice(
    session: &std::sync::Arc<tokio::sync::Mutex<Session>>,
    newest_message_id: i64,
) {
    let mut s = session.lock().await;
    s.coordination_model_watermark = s.coordination_model_watermark.max(newest_message_id);
}

/// Returns `true` when anything was pruned, so the caller can reset
/// loop-detector novelty windows that referenced the removed results.
async fn microcompact_agent_history(
    messages: &mut [Message],
    session: &std::sync::Arc<tokio::sync::Mutex<Session>>,
) -> bool {
    let cfg = {
        let guard = session.lock().await;
        guard.cfg.tool_output.clone()
    };
    let stats = crate::tool_output::microcompact_history(messages, &cfg).await;
    if stats.results_pruned == 0 && stats.arguments_pruned == 0 {
        return false;
    }

    let (analytics, external_session_id) = {
        let mut guard = session.lock().await;
        if stats.clear_read_cache {
            guard.read_cache.clear();
        } else {
            for path in &stats.evicted_read_paths {
                let resolved = guard.resolve_path(path.to_string_lossy().as_ref());
                guard.invalidate_read_cache(&resolved);
            }
        }
        (guard.analytics.clone(), guard.external_session_id.clone())
    };
    if let Some(analytics) = analytics {
        analytics.record_async(crate::analytics::ToolCallRecord {
            tool_name: "context:microcompact".to_string(),
            command: None,
            request_tokens: 0,
            response_tokens: 0,
            saved_tokens: i64::try_from(stats.estimated_tokens_saved).unwrap_or(i64::MAX),
            savings_pct: 0.0,
            exec_time_ms: 0,
            was_redirect: false,
            was_filtered: true,
            read_dedup: false,
            batch_size: u32::try_from(stats.results_pruned.saturating_add(stats.arguments_pruned))
                .unwrap_or(u32::MAX),
            external_session_id,
        });
    }
    true
}

/// Record one loop-detector event (`steer` or `circuit_breaker`) in analytics
/// (vikunja #1197). Mirrors the `context:microcompact` convention.
async fn record_loop_detector_event(
    session: &std::sync::Arc<tokio::sync::Mutex<Session>>,
    kind: &str,
    stats: crate::loop_detector::DetectorStats,
) {
    let (analytics, external_session_id) = {
        let guard = session.lock().await;
        (guard.analytics.clone(), guard.external_session_id.clone())
    };
    if let Some(analytics) = analytics {
        analytics.record_async(crate::analytics::ToolCallRecord {
            tool_name: "context:loop_detector".to_string(),
            command: Some(kind.to_string()),
            request_tokens: 0,
            response_tokens: 0,
            saved_tokens: 0,
            savings_pct: 0.0,
            exec_time_ms: 0,
            was_redirect: false,
            was_filtered: false,
            read_dedup: false,
            batch_size: stats.max_pair_repeats,
            external_session_id,
        });
    }
}

async fn record_agent_tool_output_savings(
    session: &std::sync::Arc<tokio::sync::Mutex<Session>>,
    tool_name: &str,
    visible_chars: usize,
    saved_tokens: i64,
    savings_pct: f64,
) {
    if saved_tokens <= 0 {
        return;
    }
    let (analytics, external_session_id) = {
        let guard = session.lock().await;
        (guard.analytics.clone(), guard.external_session_id.clone())
    };
    if let Some(analytics) = analytics {
        analytics.record_async(crate::analytics::ToolCallRecord {
            tool_name: "context:tool_output".to_string(),
            command: Some(tool_name.to_string()),
            request_tokens: 0,
            response_tokens: crate::analytics::estimate_tokens(visible_chars),
            saved_tokens,
            savings_pct,
            exec_time_ms: 0,
            was_redirect: false,
            was_filtered: true,
            read_dedup: false,
            batch_size: 1,
            external_session_id,
        });
    }
}

/// Record one agent-loop tool call in analytics (vikunja #1232).
///
/// Direct tool calls used to be invisible: only `script.rs` (as `script:*`) and
/// `mcp_bridge.rs` wrote per-tool rows, so scripted work was counted while
/// direct work was not. That gap is *biased* rather than merely incomplete — it
/// makes an agent look as though it always batches, which is precisely the
/// signal #1230 measures.
///
/// Mirrors `mcp::dispatch_tool` so the two frontends cannot drift: the row is
/// keyed by the plain tool name and carries the same fields. Two callers are
/// deliberately excluded, matching the MCP path:
///
/// - `execute_script`, which `handle_call_tool_request` returns early for
///   before reaching `dispatch_tool`; its sandbox ops are recorded individually
///   as `script:*`, and a parent row here would double-count them.
/// - remote MCP tools (`mcp__…`), already recorded by the bridge.
async fn record_agent_tool_call(
    session: &std::sync::Arc<tokio::sync::Mutex<Session>>,
    tool_name: &str,
    input: &Value,
    outcome: &crate::observability::ToolOutcome,
    exec_time_ms: u64,
) {
    if tool_name == EXECUTE_SCRIPT_TOOL || tool_name.starts_with(REMOTE_TOOL_PREFIX) {
        return;
    }
    let (analytics, external_session_id) = {
        let guard = session.lock().await;
        (guard.analytics.clone(), guard.external_session_id.clone())
    };
    let Some(analytics) = analytics else {
        return;
    };
    // `outcome` carries token estimates rather than the raw char counts
    // `compute_savings` takes, so derive the percentage from the same two
    // quantities it would have used: saved, and the pre-trim total.
    let saved = outcome.saved_tokens_est.max(0);
    let raw_tokens = outcome.response_tokens_est.saturating_add(saved as u64);
    let savings_pct = if saved > 0 && raw_tokens > 0 {
        saved as f64 / raw_tokens as f64 * 100.0
    } else {
        0.0
    };
    analytics.record_async(crate::analytics::ToolCallRecord {
        tool_name: tool_name.to_string(),
        command: crate::mcp::analytics_command(tool_name, input),
        request_tokens: outcome.request_tokens_est,
        response_tokens: outcome.response_tokens_est,
        saved_tokens: outcome.saved_tokens_est,
        savings_pct,
        exec_time_ms,
        was_redirect: outcome.redirect,
        was_filtered: outcome.filtered,
        read_dedup: outcome.read_dedup,
        batch_size: outcome.batch_size.min(u32::MAX as u64) as u32,
        external_session_id,
    });
}

/// Default cap on automatic continuations after a `max_tokens` stop within a
/// single user turn. `0` keeps the historical behavior (return terminal
/// `MaxTokens`); operators opt in via `DAIMONOS_AGENT_AUTO_CONTINUE` or
/// [`AgentConfig::auto_continue_budget`].
pub const DEFAULT_AUTO_CONTINUE_BUDGET: u32 = 0;

/// Cap on the working-tree diff injected into the #1235 reformatter prompt.
/// Enough to correlate a failure with a recent edit; not so much that a large
/// refactor makes the summarisation call expensive.
const REFORMAT_DIFF_MAX_CHARS: usize = 8_000;

/// Injected as a trailing user turn when a *plain-text* turn is auto-continued
/// after a `max_tokens` stop. Without it the partial assistant turn would be
/// followed by another assistant turn on the next generation, and providers such
/// as Anthropic reject consecutive assistant messages. Terse to spend near-zero
/// tokens.
const AUTO_CONTINUE_NUDGE: &str = "Continue exactly where the previous message was cut off. Do not repeat earlier output; if the remaining work is large, continue in smaller steps.";

/// Parse `DAIMONOS_AGENT_AUTO_CONTINUE` as the auto-continue cap. Absent or
/// unparseable yields `None`, leaving the config field / built-in default to
/// decide.
fn auto_continue_budget_from_env() -> Option<u32> {
    let raw = std::env::var("DAIMONOS_AGENT_AUTO_CONTINUE").ok()?;
    let trimmed = raw.trim();
    match trimmed.parse::<u32>() {
        Ok(n) => Some(n),
        Err(_) => {
            tracing::warn!(
                target: "daimonos::agent",
                value = %trimmed,
                "ignoring unparseable DAIMONOS_AGENT_AUTO_CONTINUE (want a non-negative integer); using config/default"
            );
            None
        }
    }
}

pub async fn run(
    provider: &dyn LlmProvider,
    session: std::sync::Arc<tokio::sync::Mutex<Session>>,
    initial_messages: Vec<Message>,
    config: &AgentConfig,
) -> AgentResult {
    let mut messages = initial_messages;
    let mut total_usage = Usage::default();

    // Item 3: bounded auto-continue after a `max_tokens` truncation. Resolved
    // once per turn — explicit config wins, then the env override, then the
    // built-in default (0 = off).
    let auto_continue_budget = config
        .auto_continue_budget
        .or_else(auto_continue_budget_from_env)
        .unwrap_or(DEFAULT_AUTO_CONTINUE_BUDGET);
    let mut auto_continues_used: u32 = 0;
    // Deterministic retry-storm detection (vikunja #1197). Detector state is
    // scoped to this `run` call, so a new user task starts with fresh windows.
    let mut loop_detector = {
        let cfg = { session.lock().await.cfg.clone() };
        if cfg.loop_detector.enabled {
            let template = crate::prompts::loop_steer(&cfg).await;
            Some(crate::loop_detector::LoopDetector::new(
                cfg.loop_detector.clone(),
                &template,
            ))
        } else {
            None
        }
    };
    // Corrective hint emitted by the detector; consumed by the NEXT model
    // request as ephemeral system context, never as fake history.
    let mut pending_steer: Option<String> = None;
    // True only for the generation immediately following a `max_tokens`
    // continuation; consumed each iteration so later non-continuation
    // generations in the same turn regain the caller's thinking level.
    let mut force_thinking_off = false;
    // Batch-adoption accounting (#1230). `script_ops_max` — the ops dispatched
    // by the largest single script — is the discriminator: >= 2 means a script
    // collapsed N round-trips into 1. Accumulated across the run so each token
    // log line carries the totals as of that generation.
    let mut script_ops_total = 0usize;
    let mut script_ops_max = 0usize;
    // #1240: `Some` once a transient failure exhausted its retries and the loop
    // advanced along `provider_retry.failover_models`. Sticky for the rest of
    // the turn.
    let mut failover_model: Option<String> = None;

    loop {
        // Deterministically shed old successful tool context before every
        // provider call. This runs inside a single user turn, where ADR-002's
        // between-turn compaction cannot help.
        let pruned = microcompact_agent_history(&mut messages, &session).await;
        if pruned {
            // Fingerprinted results the model could "see" may be gone now;
            // stale novelty windows must not accuse it of re-reading them.
            if let Some(detector) = loop_detector.as_mut() {
                detector.on_context_pruned();
            }
        }

        // Safe boundary: no provider stream or tool call is active here. This
        // runs before the initial generation and after complete tool-result
        // batches; the notice is ephemeral system context, not fake history.
        let coordination_notice = coordination_model_notice(&session).await;
        let notice_text = coordination_notice.as_ref().map(|(text, _)| text.as_str());
        // Loop-detector steer rides the same ephemeral channel as the
        // coordination notice: appended to system for exactly one generation.
        let steer_text = pending_steer.take();
        let system = {
            let parts: Vec<&str> = [config.system.as_deref(), notice_text, steer_text.as_deref()]
                .into_iter()
                .flatten()
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n\n"))
            }
        };
        let ctx = Context {
            messages: messages.clone(),
            system,
            tools: config.tools.clone(),
            stable_prefix_len: 0,
        };
        // On a continuation after a `max_tokens` stop, reissue with thinking
        // forced off so reasoning cannot re-consume the output budget and stall
        // progress; otherwise use the caller's opts unchanged (item 3).
        let continuation_opts;
        let opts: &CompleteOpts = if force_thinking_off || failover_model.is_some() {
            continuation_opts = CompleteOpts {
                thinking: if force_thinking_off {
                    ThinkingLevel::Off
                } else {
                    config.opts.thinking.clone()
                },
                // #1240: once failed over, every later generation in this turn
                // stays on the substitute model. Switching back mid-turn would
                // re-enter a provider already known to be degraded.
                model: failover_model
                    .clone()
                    .unwrap_or_else(|| config.opts.model.clone()),
                ..config.opts.clone()
            };
            &continuation_opts
        } else {
            &config.opts
        };
        // Consumed for this generation only.
        force_thinking_off = false;
        let ordinal = config
            .generation_ordinal
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let generation =
            crate::observability::GenerationSpan::new(crate::observability::GenerationMetadata {
                kind: "agent",
                model: &opts.model,
                max_tokens: opts.max_tokens,
                thinking: opts.thinking.clone(),
                temperature: opts.temperature,
                ordinal,
                tools_exposed: ctx.tools.len(),
                stable_prefix_len: ctx.stable_prefix_len,
            });
        // Transient failures are retried in place with bounded backoff (#1240).
        // The retry sits inside the generation so one logical model turn stays
        // one generation: a 5xx bills no tokens, so there is nothing to
        // attribute to a separate attempt.
        let mut retries = 0u32;
        let resp = loop {
            let attempt = match &config.on_stream_event {
                Some(hook) => {
                    provider
                        .stream(&ctx, opts, &mut |ev| {
                            generation.mark_first_token();
                            hook(ev);
                        })
                        .instrument(generation.span().clone())
                        .await
                }
                None => {
                    provider
                        .stream(&ctx, opts, &mut |_| {
                            generation.mark_first_token();
                        })
                        .instrument(generation.span().clone())
                        .await
                }
            };
            if !attempt.retryable || retries >= config.provider_retry.max_attempts {
                break attempt;
            }
            retries += 1;
            let delay = retry_backoff(config.provider_retry.base_delay, retries);
            tracing::warn!(
                target: "daimonos::agent",
                attempt = retries,
                max_attempts = config.provider_retry.max_attempts,
                delay_ms = delay.as_millis() as u64,
                // Provider-authored text, already bounded by the adapter. Logged
                // so an operator can tell a rate limit from an outage.
                error = attempt.error_message.as_deref().unwrap_or("unknown"),
                "transient provider error; retrying after backoff"
            );
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
        };
        generation.finish(&resp);
        // Two-phase notice delivery: only advance the watermark after a real
        // provider response consumed the notice. Provider Error/Aborted leaves
        // it pending so the next generation retries it (#1063/codeJung).
        if !matches!(resp.stop_reason, StopReason::Error | StopReason::Aborted) {
            if let Some((_, newest_message_id)) = coordination_notice.as_ref() {
                acknowledge_coordination_model_notice(&session, *newest_message_id).await;
            }
        }
        total_usage = accumulate_usage(total_usage, resp.usage.clone());
        if let Some(log_cfg) = &config.token_log {
            let context_composition = crate::context_metrics::measure_context(&ctx);
            let response_tool_calls = resp
                .content
                .iter()
                .filter(|block| matches!(block, ContentBlock::ToolCall { .. }))
                .count();
            log_token_usage(
                log_cfg,
                &config.opts.model,
                &resp.usage,
                Some(&GenerationLogMetadata {
                    kind: "agent",
                    ordinal,
                    stop_reason: &resp.stop_reason,
                    response_tool_calls,
                    context: &context_composition,
                    script_ops_total,
                    script_ops_max,
                }),
            );
        }

        // Assistant turn appended BEFORE tool results (Anthropic API requirement)
        messages.push(Message {
            role: Role::Assistant,
            content: resp.content.clone(),
        });

        match resp.stop_reason {
            StopReason::MaxTokens
                if auto_continue_budget > 0 && auto_continues_used < auto_continue_budget =>
            {
                // Item 3: auto-continue instead of dead-stopping to the client.
                // Close any truncated (orphan) tool call so history stays valid
                // and is NOT executed; a plain-text truncation leaves the partial
                // assistant turn in place as a continuation seed. The next
                // generation runs with thinking off (`continuation_opts`) so
                // reasoning cannot re-consume the budget and stall. Bounded by
                // `auto_continue_budget`; once it is exhausted the next
                // `MaxTokens` falls through to the terminal arm below and still
                // returns `MaxTokens` to the caller.
                auto_continues_used += 1;
                force_thinking_off = true;
                tracing::info!(
                    target: "daimonos::agent",
                    event = "max_tokens_auto_continue",
                    auto_continue = auto_continues_used,
                    budget = auto_continue_budget,
                    "continuing after a max_tokens truncation"
                );
                // Decide the truncation shape BEFORE close_orphan_tool_calls
                // mutates it: a truncated tool call gets a synthetic user
                // tool_result (below) and already alternates; a plain-text
                // truncation leaves the partial assistant turn last and needs a
                // minimal user turn so roles alternate — providers such as
                // Anthropic reject consecutive assistant messages, which would
                // otherwise accumulate across continuations and break the next
                // real user turn.
                let text_only_truncation = !has_orphan_tool_calls(&messages);
                messages = close_orphan_tool_calls(messages);
                if text_only_truncation {
                    messages.push(Message::user(AUTO_CONTINUE_NUDGE));
                }
                continue;
            }
            // #1240: same-model retries are spent and the failure is still
            // transient — advance the chain rather than surfacing the error.
            // Placed before the terminal arm so only transient failures with a
            // configured next model divert; everything else falls through
            // unchanged.
            StopReason::Error
                if resp.retryable
                    && next_failover_model(&config.provider_retry.failover_models, &opts.model)
                        .is_some() =>
            {
                let from = opts.model.clone();
                // Safe: the guard above proved a next model exists.
                let to = next_failover_model(&config.provider_retry.failover_models, &from)
                    .expect("guard proved a next model exists")
                    .to_string();
                tracing::warn!(
                    target: "daimonos::agent",
                    event = "provider_failover",
                    from = %from,
                    to = %to,
                    error = resp.error_message.as_deref().unwrap_or("unknown"),
                    "provider retries exhausted; failing over to the next model"
                );
                // Never silent: the `provider_failover` event above is the
                // durable record. A *frontend-visible* notice (ACP/TUI card
                // saying the turn continued on a substitute model, and that the
                // switch invalidated prompt cache) needs a new hook threaded
                // through every frontend, so it is deliberately deferred rather
                // than half-wired here — see #1240.
                failover_model = Some(to);
                continue;
            }
            StopReason::EndTurn
            | StopReason::MaxTokens
            | StopReason::Refusal
            | StopReason::Aborted
            | StopReason::Error => {
                return AgentResult {
                    messages: close_orphan_tool_calls(messages),
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

                let output_cfg = {
                    let guard = session.lock().await;
                    guard.cfg.tool_output.clone()
                };
                let mut tool_results = Vec::new();
                let mut round_observations = Vec::new();
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
                    // Wall time for this call's analytics row (vikunja #1232),
                    // matching what `mcp::dispatch_tool` measures on the MCP path.
                    let tool_started = std::time::Instant::now();
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
                    let mut tool_span = dispatched.then(|| {
                        crate::observability::ToolSpan::new(
                            &name,
                            crate::observability::tool_kind(&name),
                        )
                    });
                    let (mut content, is_error, mut outcome) = if name == EXECUTE_SCRIPT_TOOL {
                        // execute_script shares the live session with its
                        // Starlark sandbox thread, so the loop hands it an
                        // `Arc<Mutex<Session>>` clone rather than a `&mut`.
                        let code = input
                            .get("code")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let timeout_secs = crate::script::bounded_timeout_secs(
                            input.get("timeout").and_then(serde_json::Value::as_i64),
                        );
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
                        // Batch-adoption accounting (#1230). Read after the run
                        // so a script that errored mid-way still contributes the
                        // ops it completed — a partially-successful batch is
                        // still evidence the model attempted one.
                        let ops = op_count.load(std::sync::atomic::Ordering::Relaxed);
                        script_ops_total = script_ops_total.saturating_add(ops);
                        script_ops_max = script_ops_max.max(ops);
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
                                // Plugin tools (git, cargo, shellcheck, …) and meta
                                // tools (list_all_tools, kgl_query, workspace_info,
                                // …) carry no opcode mapping, so the facade declines
                                // them. They are implemented once in the MCP
                                // dispatcher; route there rather than reimplementing,
                                // so the two frontends cannot drift (vikunja 1112).
                                let local = {
                                    let mut session_guard = session.lock().await;
                                    crate::mcp::dispatch_local_tool(
                                        &mut session_guard,
                                        &name,
                                        &input,
                                    )
                                    .await
                                };
                                if let Some((content, is_error, meta)) = local {
                                    let content = if name == LIST_ALL_TOOLS_TOOL {
                                        append_remote_tools_to_catalog(content, &config.tools)
                                    } else {
                                        content
                                    };
                                    let response_chars = content.len();
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
                                    if tool_span.is_none() {
                                        // These tools carry no opcode mapping,
                                        // so open their span here and close it
                                        // only after the shared output boundary.
                                        tool_span = Some(crate::observability::ToolSpan::new(
                                            &name,
                                            dispatch_tool_kind(&name),
                                        ));
                                    }
                                    (content, is_error, Some(outcome))
                                } else {
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
                                                format!(
                                                    "tool '{name}' not available in agent mode"
                                                ),
                                                true,
                                                None,
                                            )
                                        }
                                    }
                                }
                            }
                        }
                    };
                    // after_tool_call hook
                    if let Some(hook) = &config.after_tool_call {
                        if matches!(hook(&info, &content, is_error), AfterHookResult::Terminate) {
                            terminate = true;
                        }
                    }

                    // Every agent-visible text result crosses one shared
                    // boundary after UI hooks inspect the complete output but
                    // before model history retains it (vikunja #1193).
                    //
                    // #1235: for allowlisted noisy verifier tools, an opt-in
                    // paid LLM pass replaces that bounding with an actionable
                    // reformatting. Off by default; `reformat_text` degrades to
                    // `bound_text` on any failure, so this can only ever change
                    // the *shape* of a successful result, never lose one.
                    let reformatter = config.subcall_provider.as_ref().filter(|_| {
                        crate::tool_output::should_reformat(
                            &output_cfg.reformat,
                            &name,
                            is_error,
                            content.chars().count(),
                        )
                    });
                    let bounded = match reformatter {
                        Some(provider) => {
                            let model = output_cfg
                                .reformat
                                .model
                                .clone()
                                .or_else(|| {
                                    config
                                        .compaction
                                        .as_ref()
                                        .and_then(|c| c.summary_model.clone())
                                })
                                .unwrap_or_else(|| config.opts.model.clone());
                            let workspace = { session.lock().await.workspace.clone() };
                            // Bounded and best-effort: a missing diff only makes
                            // the summary less specific.
                            let diff = crate::tool_output::working_tree_diff(
                                &workspace,
                                REFORMAT_DIFF_MAX_CHARS,
                            )
                            .await;
                            // Name + first description line only: the catalog is
                            // context for phrasing next steps, not documentation.
                            let catalog: Vec<(&str, &str)> = config
                                .tools
                                .iter()
                                .map(|t| (t.name.as_str(), t.description.as_str()))
                                .collect();
                            crate::tool_output::reformat_text(
                                &output_cfg,
                                &name,
                                content,
                                provider.as_ref(),
                                &model,
                                &info.input,
                                diff.as_deref(),
                                &catalog,
                            )
                            .await
                        }
                        None => crate::tool_output::bound_text(&output_cfg, &name, content).await,
                    };
                    let was_offloaded = bounded.output_path.is_some();
                    if was_offloaded && name == "read_file" {
                        let mut guard = session.lock().await;
                        if let Some(path) = input.get("path").and_then(Value::as_str) {
                            let resolved = guard.resolve_path(path);
                            guard.invalidate_read_cache(&resolved);
                        } else {
                            guard.read_cache.clear();
                        }
                    }
                    let (additional_saved, savings_pct) = crate::analytics::compute_savings(
                        bounded.original_chars,
                        bounded.visible_chars,
                    );
                    record_agent_tool_output_savings(
                        &session,
                        &name,
                        bounded.visible_chars,
                        additional_saved,
                        savings_pct,
                    )
                    .await;
                    if let Some(outcome) = &mut outcome {
                        outcome.response_tokens_est =
                            crate::analytics::estimate_tokens(bounded.visible_chars);
                        outcome.saved_tokens_est =
                            outcome.saved_tokens_est.saturating_add(additional_saved);
                        outcome.filtered |= was_offloaded;
                    }
                    content = bounded.content;

                    // Analytics row for this call (vikunja #1232). Emitted here,
                    // after output-bounding has finalized `outcome`, so every
                    // dispatch branch — facade, local MCP dispatch, plan tool,
                    // remote — converges on one record and the numbers match
                    // what the model actually saw.
                    if let Some(outcome) = outcome.as_ref() {
                        record_agent_tool_call(
                            &session,
                            &name,
                            &input,
                            outcome,
                            tool_started.elapsed().as_millis() as u64,
                        )
                        .await;
                    }

                    if let Some(tool_span) = tool_span {
                        let status = if is_error {
                            crate::observability::ToolStatus::Error
                        } else {
                            crate::observability::ToolStatus::Success
                        };
                        tool_span.finish(status, outcome.unwrap_or_default());
                    }

                    // Observe the MODEL-VISIBLE result (post output-bounding):
                    // a repeated truncation placeholder is exactly the kind of
                    // no-progress signal the detector must see (vikunja #1197).
                    if loop_detector.is_some() {
                        round_observations.push(crate::loop_detector::CallObservation::new(
                            &name, &input, is_error, &content,
                        ));
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

                // One detector round per complete parallel batch (#1197). A
                // steer becomes ephemeral system context on the next request;
                // the circuit breaker ends the turn BEFORE paying for another
                // generation, with all tool results already paired above.
                if let Some(detector) = loop_detector.as_mut() {
                    match detector.observe_round(&round_observations) {
                        crate::loop_detector::RoundVerdict::Proceed => {}
                        crate::loop_detector::RoundVerdict::Steer(text) => {
                            let stats = detector.stats();
                            tracing::info!(
                                target: "daimonos::agent",
                                event = "loop_detector_steer",
                                steers_emitted = stats.steers_emitted,
                                steers_suppressed = stats.steers_suppressed,
                                max_pair_repeats = stats.max_pair_repeats,
                                "injecting corrective steer after repeated no-progress tool rounds"
                            );
                            record_loop_detector_event(&session, "steer", stats).await;
                            pending_steer = Some(text);
                        }
                        crate::loop_detector::RoundVerdict::Break(message) => {
                            let stats = detector.stats();
                            tracing::warn!(
                                target: "daimonos::agent",
                                event = "loop_detector_circuit_breaker",
                                rounds_observed = stats.rounds_observed,
                                steers_emitted = stats.steers_emitted,
                                max_pair_repeats = stats.max_pair_repeats,
                                "stopping turn: tool retry storm exhausted the circuit breaker"
                            );
                            record_loop_detector_event(&session, "circuit_breaker", stats).await;
                            return AgentResult {
                                messages,
                                usage: total_usage,
                                stop_reason: StopReason::Aborted,
                                error_message: Some(message),
                                last_call_usage: resp.usage,
                                context_overflow: false,
                            };
                        }
                    }
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
        // Repair live in-memory history as well as loaded history. A terminal
        // provider response from an older process can leave a ToolCall without
        // its adjacent ToolResult and wedge every later request in this same
        // frontend process.
        if has_orphan_tool_calls(&self.messages) {
            self.messages = close_orphan_tool_calls(std::mem::take(&mut self.messages));
        }

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
                "Daimonos agent mail: {} new unread message(s) for {} (highest importance: {}). The agent will be notified at its next safe action boundary.",
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
                    let context_composition = crate::context_metrics::measure_context(&ctx);
                    log_token_usage(
                        log_cfg,
                        &summary_model,
                        &resp.usage,
                        Some(&GenerationLogMetadata {
                            kind: "compaction_summary",
                            ordinal,
                            stop_reason: &resp.stop_reason,
                            response_tool_calls: 0,
                            context: &context_composition,
                            // Compaction issues no tool calls, so no script ops.
                            script_ops_total: 0,
                            script_ops_max: 0,
                        }),
                    );
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

    /// Insert a synthetic assistant message at the start of the most recent
    /// user turn. Frontends can use this for turn metadata after the provider
    /// has completed, so it cannot alter that turn's request. The inserted
    /// message is part of normal persisted history.
    pub fn insert_assistant_turn_prefix(
        &mut self,
        prefix: impl Into<String>,
    ) -> Result<(), String> {
        let Some(user_index) = self.messages.iter().rposition(is_user_turn_message) else {
            return Err("cannot prefix assistant turn: session has no user turn".to_string());
        };
        self.messages
            .insert(user_index + 1, Message::assistant(prefix));
        Ok(())
    }

    /// Replace the conversation history, e.g. restoring a persisted ACP
    /// session on `session/load` after a process restart. Cumulative usage is
    /// left untouched (it's a fresh process, so there's none to preserve).
    ///
    /// History is repaired on the way in — see [`close_orphan_tool_calls`].
    pub fn set_history(&mut self, messages: Vec<Message>) {
        self.messages = close_orphan_tool_calls(messages);
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

/// Text recorded as the result of a tool call that never produced one.
pub(crate) const INTERRUPTED_TOOL_RESULT: &str =
    "Tool call interrupted: the assistant turn ended before this tool ran, so it produced \
     no result and changed nothing. Re-issue it if it is still needed.";

fn has_orphan_tool_calls(messages: &[Message]) -> bool {
    if !messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|block| matches!(block, ContentBlock::ToolCall { .. }))
    {
        return false;
    }
    let answered: HashSet<&str> = messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();
    messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|block| match block {
            ContentBlock::ToolCall { id, .. } => !answered.contains(id.as_str()),
            _ => false,
        })
}

/// Give every assistant `ToolCall` a matching `ToolResult`.
///
/// A turn killed mid-tool-call (cancel, crash, dropped stream) persists the
/// call without its result. Providers reject that shape outright — OpenAI's
/// Responses API 400s on a `function_call` with no `function_call_output` —
/// and since the gap is persisted, *every* later prompt in that session fails
/// the same way, wedging the thread permanently. Repairing on load turns a
/// dead session into one that just sees a failed tool call and moves on.
///
/// Synthetic results are inserted directly after the message that made the
/// call, so ordering stays valid for providers that require the pairing to be
/// adjacent. `ToolResult`-only messages are not user turns
/// ([`is_user_turn_message`]), so turn indexing is unaffected.
fn close_orphan_tool_calls(messages: Vec<Message>) -> Vec<Message> {
    let answered: HashSet<&str> = messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();

    let orphans: Vec<Vec<String>> = messages
        .iter()
        .map(|message| {
            if message.role != Role::Assistant {
                return Vec::new();
            }
            message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolCall { id, .. } if !answered.contains(id.as_str()) => {
                        Some(id.clone())
                    }
                    _ => None,
                })
                .collect()
        })
        .collect();

    if orphans.iter().all(Vec::is_empty) {
        return messages;
    }

    let mut repaired = Vec::with_capacity(messages.len() + 1);
    for (message, ids) in messages.into_iter().zip(orphans) {
        repaired.push(message);
        if ids.is_empty() {
            continue;
        }
        tracing::warn!(
            target: "daimonos::agent",
            event = "orphan_tool_calls_closed",
            tool_call_ids = ?ids,
        );
        repaired.push(Message {
            role: Role::User,
            content: ids
                .into_iter()
                .map(|tool_use_id| ContentBlock::ToolResult {
                    tool_use_id,
                    content: INTERRUPTED_TOOL_RESULT.to_string(),
                    is_error: true,
                })
                .collect(),
        });
    }
    repaired
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
        | ContentBlock::Thinking(_)
        | ContentBlock::ProviderState { .. } => false,
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
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};

    // --- MockProvider ---

    /// vikunja #1235 end to end: with the reformatter enabled and a matched
    /// tool, the *model-visible* result in history is the reformatting — not
    /// the raw output and not a head/tail truncation. Proves the pass is
    /// actually reachable through `run()`, not just unit-testable.
    #[tokio::test]
    async fn enabled_reformatter_replaces_a_matched_tools_output_in_history() {
        let dir = tempfile::tempdir().unwrap();
        let noisy = "FAILED tests/test_x.py - AssertionError\n".repeat(300);
        std::fs::write(dir.path().join("big.txt"), &noisy).unwrap();

        let mut cfg = Config::default();
        cfg.tool_output.directory =
            Some(dir.path().join("tool-output").to_string_lossy().to_string());
        cfg.tool_output.reformat = crate::config::ReformatConfig {
            enabled: true,
            tools: vec!["read_file".into()],
            min_chars: 100,
            model: Some("cheap".into()),
            max_input_chars: 10_000,
        };
        let session = Session::new(dir.path().to_path_buf(), Arc::new(cfg));

        struct Reformatter;
        #[async_trait]
        impl LlmProvider for Reformatter {
            async fn complete(&self, _c: &Context, _o: &CompleteOpts) -> LlmResponse {
                end_turn_resp_with_text("300 identical failures: test_x AssertionError")
            }
        }

        let provider = MockProvider::new(vec![
            tool_call_resp("t1", "read_file", json!({"path": "big.txt"})),
            end_turn_resp(),
        ]);
        let config = AgentConfig {
            subcall_provider: Some(Arc::new(Reformatter)),
            ..AgentConfig::default()
        };

        let result = run(
            &provider,
            shared(session),
            vec![Message::user("run the tests")],
            &config,
        )
        .await;

        let tool_results: Vec<String> = result
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        let joined = tool_results.join("\n");

        assert!(
            joined.contains("300 identical failures"),
            "history should carry the reformatting, got: {}",
            &joined[..joined.len().min(300)]
        );
        assert!(
            !joined.contains(&noisy),
            "the raw dump must not be in model history"
        );
        // Raw stays recoverable on disk.
        assert!(joined.contains("full output saved to"));
    }

    /// The same run with the reformatter disabled must not call the sub-model at
    /// all — this is the "zero cost when off" guarantee.
    #[tokio::test]
    async fn disabled_reformatter_never_calls_the_sub_model() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("big.txt"),
            "FAILED\n".repeat(300).as_bytes(),
        )
        .unwrap();

        let mut cfg = Config::default();
        cfg.tool_output.directory =
            Some(dir.path().join("tool-output").to_string_lossy().to_string());
        // reformat left at its default: disabled.
        let session = Session::new(dir.path().to_path_buf(), Arc::new(cfg));

        struct MustNotBeCalled {
            called: Arc<std::sync::atomic::AtomicUsize>,
        }
        #[async_trait]
        impl LlmProvider for MustNotBeCalled {
            async fn complete(&self, _c: &Context, _o: &CompleteOpts) -> LlmResponse {
                self.called
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                end_turn_resp_with_text("should never happen")
            }
        }
        let called = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let provider = MockProvider::new(vec![
            tool_call_resp("t1", "read_file", json!({"path": "big.txt"})),
            end_turn_resp(),
        ]);
        let config = AgentConfig {
            subcall_provider: Some(Arc::new(MustNotBeCalled {
                called: Arc::clone(&called),
            })),
            ..AgentConfig::default()
        };

        run(
            &provider,
            shared(session),
            vec![Message::user("go")],
            &config,
        )
        .await;

        assert_eq!(
            called.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "disabled reformatter must cost nothing"
        );
    }

    #[test]
    fn next_failover_model_walks_forward_and_stops_at_the_end() {
        let chain: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(next_failover_model(&chain, "a"), Some("b"));
        assert_eq!(next_failover_model(&chain, "b"), Some("c"));
        // Last entry: chain exhausted, so the error must surface.
        assert_eq!(next_failover_model(&chain, "c"), None);
        // A model outside the chain has no defined successor — better to
        // surface the error than to guess an entry point.
        assert_eq!(next_failover_model(&chain, "unlisted"), None);
        assert_eq!(next_failover_model(&[], "a"), None);
    }

    /// Records every model it was asked for, and fails everything except one.
    /// Lets a test prove *which* model each attempt used, not merely that the
    /// turn eventually succeeded.
    struct FailoverProbe {
        healthy_model: &'static str,
        seen: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl LlmProvider for FailoverProbe {
        async fn complete(&self, _ctx: &Context, opts: &CompleteOpts) -> LlmResponse {
            self.seen.lock().unwrap().push(opts.model.clone());
            if opts.model == self.healthy_model {
                end_turn_resp()
            } else {
                LlmResponse::retryable_error(format!("API 529: {} overloaded", opts.model))
            }
        }
    }

    /// vikunja #1240: when same-model retries are exhausted on a transient
    /// failure, advance to the next model in the configured chain rather than
    /// surfacing the error.
    #[tokio::test]
    async fn exhausted_retries_fail_over_to_the_next_model_in_the_chain() {
        let provider = FailoverProbe {
            healthy_model: "model-c",
            seen: Mutex::new(Vec::new()),
        };
        let config = AgentConfig {
            opts: CompleteOpts {
                model: "model-a".into(),
                ..CompleteOpts::default()
            },
            provider_retry: ProviderRetryConfig {
                max_attempts: 1,
                base_delay: std::time::Duration::ZERO,
                failover_models: vec!["model-a".into(), "model-b".into(), "model-c".into()],
            },
            ..AgentConfig::default()
        };

        let dir = tempfile::tempdir().unwrap();
        let result = run(
            &provider,
            shared(session_in(dir.path())),
            vec![Message::user("hi")],
            &config,
        )
        .await;

        assert_eq!(result.stop_reason, StopReason::EndTurn);
        let seen = provider.seen.lock().unwrap().clone();
        // a twice (initial + 1 retry), b twice, then c succeeds first try.
        assert_eq!(
            seen,
            vec!["model-a", "model-a", "model-b", "model-b", "model-c"],
            "should exhaust each model's retries before advancing"
        );
    }

    /// Failover is opt-in: with no chain configured, behaviour is exactly the
    /// pre-#1240 surface — the transient error is returned after retries.
    #[tokio::test]
    async fn no_chain_configured_means_no_failover() {
        let provider = FailoverProbe {
            healthy_model: "model-c",
            seen: Mutex::new(Vec::new()),
        };
        let config = AgentConfig {
            opts: CompleteOpts {
                model: "model-a".into(),
                ..CompleteOpts::default()
            },
            provider_retry: ProviderRetryConfig {
                max_attempts: 1,
                base_delay: std::time::Duration::ZERO,
                failover_models: vec![],
            },
            ..AgentConfig::default()
        };

        let dir = tempfile::tempdir().unwrap();
        let result = run(
            &provider,
            shared(session_in(dir.path())),
            vec![Message::user("hi")],
            &config,
        )
        .await;

        assert_eq!(result.stop_reason, StopReason::Error);
        assert_eq!(
            provider.seen.lock().unwrap().clone(),
            vec!["model-a", "model-a"],
            "only the active model, initial + one retry"
        );
    }

    /// A fatal error must not consume the chain — failing over with the same
    /// bad request just multiplies one misconfiguration across every model.
    #[tokio::test]
    async fn fatal_errors_do_not_trigger_failover() {
        struct AlwaysFatal {
            seen: Mutex<Vec<String>>,
        }
        #[async_trait]
        impl LlmProvider for AlwaysFatal {
            async fn complete(&self, _ctx: &Context, opts: &CompleteOpts) -> LlmResponse {
                self.seen.lock().unwrap().push(opts.model.clone());
                LlmResponse::error("API 401: invalid api key")
            }
        }
        let provider = AlwaysFatal {
            seen: Mutex::new(Vec::new()),
        };
        let config = AgentConfig {
            opts: CompleteOpts {
                model: "model-a".into(),
                ..CompleteOpts::default()
            },
            provider_retry: ProviderRetryConfig {
                max_attempts: 2,
                base_delay: std::time::Duration::ZERO,
                failover_models: vec!["model-a".into(), "model-b".into()],
            },
            ..AgentConfig::default()
        };

        let dir = tempfile::tempdir().unwrap();
        let result = run(
            &provider,
            shared(session_in(dir.path())),
            vec![Message::user("hi")],
            &config,
        )
        .await;

        assert_eq!(result.stop_reason, StopReason::Error);
        assert_eq!(
            provider.seen.lock().unwrap().clone(),
            vec!["model-a"],
            "one call only: no retry and no failover on a fatal error"
        );
    }

    #[test]
    fn retry_backoff_doubles_and_stays_bounded() {
        use std::time::Duration;
        let base = Duration::from_millis(500);
        assert_eq!(retry_backoff(base, 1), Duration::from_millis(500));
        assert_eq!(retry_backoff(base, 2), Duration::from_secs(1));
        assert_eq!(retry_backoff(base, 3), Duration::from_secs(2));
        // The shift is clamped so a misconfigured max_attempts cannot overflow
        // into an effectively-infinite sleep.
        assert_eq!(retry_backoff(base, 7), Duration::from_secs(32));
        assert_eq!(retry_backoff(base, 99), Duration::from_secs(32));
        // Zero base stays zero at every attempt (what tests rely on).
        assert!(retry_backoff(Duration::ZERO, 4).is_zero());
    }

    /// vikunja #1240: a transient provider failure must not abort the turn.
    /// The loop retries the same request with bounded backoff; the mock's queue
    /// is drained only if every attempt actually happened.
    #[tokio::test]
    async fn transient_provider_errors_are_retried_until_one_succeeds() {
        let provider = MockProvider::new(vec![
            LlmResponse::retryable_error("API 529: overloaded"),
            LlmResponse::retryable_error("API 503: upstream"),
            end_turn_resp(),
        ]);
        let config = AgentConfig {
            provider_retry: ProviderRetryConfig {
                max_attempts: 2,
                base_delay: std::time::Duration::ZERO,
                failover_models: vec![],
            },
            ..AgentConfig::default()
        };

        let dir = tempfile::tempdir().unwrap();
        let result = run(
            &provider,
            shared(session_in(dir.path())),
            vec![Message::user("hi")],
            &config,
        )
        .await;

        assert_eq!(
            result.stop_reason,
            StopReason::EndTurn,
            "two transient failures should have been absorbed"
        );
        assert!(
            provider.responses.lock().unwrap().is_empty(),
            "all three attempts should have been consumed"
        );
    }

    /// The other half of the rule: a fatal error is surfaced immediately. If it
    /// retried, it would mask a real misconfiguration (bad key, bad request)
    /// behind latency and burned tokens.
    #[tokio::test]
    async fn fatal_provider_errors_are_not_retried() {
        let provider = MockProvider::new(vec![
            LlmResponse::error("API 401: invalid api key"),
            end_turn_resp(),
        ]);
        let config = AgentConfig {
            provider_retry: ProviderRetryConfig {
                max_attempts: 3,
                base_delay: std::time::Duration::ZERO,
                failover_models: vec![],
            },
            ..AgentConfig::default()
        };

        let dir = tempfile::tempdir().unwrap();
        let result = run(
            &provider,
            shared(session_in(dir.path())),
            vec![Message::user("hi")],
            &config,
        )
        .await;

        assert_eq!(result.stop_reason, StopReason::Error);
        assert_eq!(
            provider.responses.lock().unwrap().len(),
            1,
            "the success response must remain unconsumed — no retry happened"
        );
    }

    /// Retries are bounded: an endlessly-failing provider surfaces the error
    /// rather than looping forever.
    #[tokio::test]
    async fn retries_are_capped_and_then_surface_the_error() {
        let provider = MockProvider::new(vec![
            LlmResponse::retryable_error("API 503: a"),
            LlmResponse::retryable_error("API 503: b"),
            LlmResponse::retryable_error("API 503: c"),
            end_turn_resp(),
        ]);
        let config = AgentConfig {
            provider_retry: ProviderRetryConfig {
                max_attempts: 2,
                base_delay: std::time::Duration::ZERO,
                failover_models: vec![],
            },
            ..AgentConfig::default()
        };

        let dir = tempfile::tempdir().unwrap();
        let result = run(
            &provider,
            shared(session_in(dir.path())),
            vec![Message::user("hi")],
            &config,
        )
        .await;

        assert_eq!(result.stop_reason, StopReason::Error);
        // 1 initial + 2 retries = 3 consumed; the success response is never reached.
        assert_eq!(provider.responses.lock().unwrap().len(), 1);
    }

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

    struct BenchmarkCaptureProvider {
        responses: Mutex<VecDeque<LlmResponse>>,
        contexts: Arc<Mutex<Vec<Vec<Message>>>>,
    }

    impl BenchmarkCaptureProvider {
        fn new(responses: Vec<LlmResponse>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(responses)),
                contexts: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn contexts_handle(&self) -> Arc<Mutex<Vec<Vec<Message>>>> {
            Arc::clone(&self.contexts)
        }
    }

    #[async_trait]
    impl LlmProvider for BenchmarkCaptureProvider {
        async fn complete(&self, ctx: &Context, _opts: &CompleteOpts) -> LlmResponse {
            self.contexts.lock().unwrap().push(ctx.messages.clone());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| LlmResponse::error("BenchmarkCaptureProvider exhausted"))
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
            retryable: false,
            content: vec![ContentBlock::Text(text.to_string())],
            stop_reason: StopReason::EndTurn,
            error_message: None,
            context_overflow: false,
            usage: mock_usage(100, 50),
        }
    }

    fn tool_call_resp(id: &str, name: &str, input: Value) -> LlmResponse {
        LlmResponse {
            retryable: false,
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

    fn bounded_session_in(dir: &std::path::Path) -> Session {
        let mut cfg = Config::default();
        cfg.tool_output.directory = Some(dir.join("tool-output").to_string_lossy().to_string());
        cfg.tool_output.max_bytes = 256;
        cfg.tool_output.max_lines = 6;
        Session::new(dir.to_path_buf(), Arc::new(cfg))
    }

    #[derive(Debug, serde::Serialize)]
    struct ControlledArmMetrics {
        model_visible_chars: usize,
        model_visible_tokens: u64,
        wall_micros: u128,
        offload_files: usize,
        pruned_results: usize,
        truncated_arguments: usize,
        recoverable_sentinels: usize,
        read_cache_entries: usize,
        output_saved_tokens: i64,
        microcompact_saved_tokens: i64,
    }

    fn controlled_tool_calls() -> Vec<ContentBlock> {
        let mut calls = vec![
            ContentBlock::ToolCall {
                id: "read-large".into(),
                name: "read_file".into(),
                input: json!({"path":"large-read.txt"}),
            },
            ContentBlock::ToolCall {
                id: "meta-large".into(),
                name: "list_all_tools".into(),
                input: json!({}),
            },
            ContentBlock::ToolCall {
                id: "script-large".into(),
                name: "execute_script".into(),
                input: json!({"code": "result = \"SCRIPT_SENTINEL\" + \"x\" * 60000"}),
            },
            ContentBlock::ToolCall {
                id: "remote-large".into(),
                name: "mcp__bench__large".into(),
                input: json!({}),
            },
            ContentBlock::ToolCall {
                id: "write-old".into(),
                name: "write_file".into(),
                input: json!({
                    "path":"written.txt",
                    "content": format!("WRITE_SENTINEL{}", "w".repeat(10_000)),
                }),
            },
            ContentBlock::ToolCall {
                id: "edit-old".into(),
                name: "edit_file".into(),
                input: json!({
                    "path":"edit.txt",
                    "edits":[
                        format!("EDIT_OLD_SENTINEL{}", "o".repeat(10_000)),
                        format!("EDIT_NEW_SENTINEL{}", "n".repeat(10_000)),
                    ],
                }),
            },
        ];
        calls.extend((0..8).map(|index| ContentBlock::ToolCall {
            id: format!("medium-{index}"),
            name: format!("mcp__bench__medium_{index}"),
            input: json!({}),
        }));
        calls.push(ContentBlock::ToolCall {
            id: "error".into(),
            name: "does_not_exist".into(),
            input: json!({}),
        });
        calls
    }

    fn controlled_tool_schemas() -> Vec<ToolSchema> {
        let mut schemas = (0..160)
            .map(|index| ToolSchema {
                name: format!("mcp__bench__schema_{index}"),
                description: format!("META_SENTINEL_{}{}", index, "d".repeat(600)),
                input_schema: json!({"type":"object","properties":{}}),
            })
            .collect::<Vec<_>>();
        schemas.push(ToolSchema {
            name: "mcp__bench__large".into(),
            description: "controlled large remote tool".into(),
            input_schema: json!({"type":"object"}),
        });
        schemas.extend((0..8).map(|index| ToolSchema {
            name: format!("mcp__bench__medium_{index}"),
            description: "controlled medium remote tool".into(),
            input_schema: json!({"type":"object"}),
        }));
        schemas
    }

    fn count_marker(messages: &[Message], marker: &str) -> usize {
        messages
            .iter()
            .flat_map(|message| &message.content)
            .filter(|block| match block {
                ContentBlock::ToolResult { content, .. } => content.contains(marker),
                ContentBlock::ToolCall { input, .. } => input.to_string().contains(marker),
                _ => false,
            })
            .count()
    }

    async fn run_controlled_tool_output_arm(disabled: bool) -> ControlledArmMetrics {
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("tool-output");
        std::fs::write(
            dir.path().join("large-read.txt"),
            format!("READ_SENTINEL\n{}", "r".repeat(60_000)),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("edit.txt"),
            format!("EDIT_OLD_SENTINEL{}", "o".repeat(10_000)),
        )
        .unwrap();

        let mut cfg = Config::default();
        cfg.tool_output.directory = Some(output_dir.to_string_lossy().to_string());
        if disabled {
            cfg.tool_output.max_bytes = usize::MAX;
            cfg.tool_output.max_lines = usize::MAX;
            cfg.tool_output.intra_turn_result_budget_tokens = u64::MAX;
            cfg.tool_output.intra_turn_keep_recent_results = usize::MAX;
            cfg.tool_output.old_argument_max_chars = usize::MAX;
        }
        let analytics = Arc::new(
            crate::analytics::AnalyticsStore::new(&dir.path().join("analytics.db"), 90).unwrap(),
        );
        let mut session = Session::new(dir.path().to_path_buf(), Arc::new(cfg));
        session.analytics = Some(Arc::clone(&analytics));
        let session = shared(session);

        let first_response = LlmResponse {
            retryable: false,
            content: controlled_tool_calls(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            context_overflow: false,
            usage: Usage::default(),
        };
        let provider =
            BenchmarkCaptureProvider::new(vec![first_response, end_turn_resp_with_text("done")]);
        let contexts = provider.contexts_handle();
        let hook_outputs = Arc::new(Mutex::new(HashMap::new()));
        let captured_outputs = Arc::clone(&hook_outputs);
        let config = AgentConfig {
            tools: controlled_tool_schemas(),
            after_tool_call: Some(Box::new(move |info, content, _| {
                captured_outputs
                    .lock()
                    .unwrap()
                    .insert(info.name.clone(), content.to_string());
                AfterHookResult::Continue
            })),
            remote_tool_dispatch: Some(Box::new(|name: &str, _input: &Value| {
                let name = name.to_string();
                Box::pin(async move {
                    if name == "mcp__bench__large" {
                        return Some(RemoteToolResult {
                            content: format!("REMOTE_LARGE_SENTINEL\n{}", "l".repeat(60_000)),
                            is_error: false,
                        });
                    }
                    name.strip_prefix("mcp__bench__medium_")
                        .map(|index| RemoteToolResult {
                            content: format!("MEDIUM_SENTINEL_{index}\n{}", "m".repeat(30_000)),
                            is_error: false,
                        })
                })
            })),
            ..AgentConfig::default()
        };

        let started = std::time::Instant::now();
        let result = run(
            &provider,
            Arc::clone(&session),
            vec![Message::user("controlled benchmark")],
            &config,
        )
        .await;
        let wall_micros = started.elapsed().as_micros();
        assert_eq!(result.stop_reason, StopReason::EndTurn);

        let visible = {
            let contexts = contexts.lock().unwrap();
            assert_eq!(contexts.len(), 2);
            contexts[1].clone()
        };
        let call_ids = visible
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::ToolCall { id, .. } => Some(id),
                _ => None,
            })
            .collect::<HashSet<_>>();
        let result_ids = visible
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id),
                _ => None,
            })
            .collect::<HashSet<_>>();
        assert_eq!(call_ids, result_ids, "tool call/result pairing changed");
        assert!(visible
            .iter()
            .flat_map(|message| &message.content)
            .any(|block| matches!(
                block,
                ContentBlock::ToolResult {
                    tool_use_id,
                    is_error: true,
                    ..
                } if tool_use_id == "error"
            )));

        assert!(
            analytics
                .wait_until_quiet(std::time::Duration::from_secs(2))
                .await
        );
        let stats = analytics.session_summary();
        let output_saved_tokens = stats
            .per_tool
            .get("context:tool_output")
            .map(|tool| tool.saved_tokens)
            .unwrap_or(0);
        let microcompact_saved_tokens = stats
            .per_tool
            .get("context:microcompact")
            .map(|tool| tool.saved_tokens)
            .unwrap_or(0);
        let files = std::fs::read_dir(&output_dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let recovered_sentinels = [
            "READ_SENTINEL",
            "META_SENTINEL",
            "SCRIPT_SENTINEL",
            "REMOTE_LARGE_SENTINEL",
        ]
        .into_iter()
        .filter(|sentinel| {
            files.iter().any(|path| {
                std::fs::read_to_string(path).is_ok_and(|content| content.contains(sentinel))
            })
        })
        .collect::<Vec<_>>();
        let recoverable_sentinels = recovered_sentinels.len();
        let read_cache_entries = session.lock().await.read_cache.len();
        let model_visible_chars = serde_json::to_string(&visible).unwrap().chars().count();
        let hook_outputs = hook_outputs.lock().unwrap();
        let meta_output = hook_outputs
            .get("list_all_tools")
            .expect("meta tool result must reach the full-content hook");
        assert!(
            meta_output.len() > 50 * 1024 && meta_output.contains("META_SENTINEL"),
            "controlled meta output was not oversized: {} bytes",
            meta_output.len()
        );

        if disabled {
            assert_eq!(files.len(), 0);
            assert_eq!(output_saved_tokens, 0);
            assert_eq!(microcompact_saved_tokens, 0);
            assert_eq!(count_marker(&visible, "argument truncated"), 0);
        } else {
            assert!(files.len() >= 4);
            assert_eq!(
                recoverable_sentinels, 4,
                "missing recoverable outputs: {recovered_sentinels:?}"
            );
            assert!(output_saved_tokens > 0);
            assert!(microcompact_saved_tokens > 0);
            assert!(count_marker(&visible, "old tool result pruned") > 0);
            assert!(count_marker(&visible, "argument truncated") >= 2);
            assert_eq!(read_cache_entries, 0);
        }

        ControlledArmMetrics {
            model_visible_chars,
            model_visible_tokens: crate::analytics::estimate_tokens(model_visible_chars),
            wall_micros,
            offload_files: files.len(),
            pruned_results: count_marker(&visible, "old tool result pruned"),
            truncated_arguments: count_marker(&visible, "argument truncated"),
            recoverable_sentinels,
            read_cache_entries,
            output_saved_tokens,
            microcompact_saved_tokens,
        }
    }

    #[tokio::test]
    #[ignore = "controlled API-free benchmark; run via benchmarks/bench-tool-output.sh"]
    async fn controlled_tool_output_benchmark() {
        const REPETITIONS: usize = 3;
        let mut baseline = Vec::with_capacity(REPETITIONS);
        let mut candidate = Vec::with_capacity(REPETITIONS);
        for _ in 0..REPETITIONS {
            baseline.push(run_controlled_tool_output_arm(true).await);
            candidate.push(run_controlled_tool_output_arm(false).await);
        }

        let mean = |values: &[ControlledArmMetrics], select: fn(&ControlledArmMetrics) -> f64| {
            values.iter().map(select).sum::<f64>() / values.len() as f64
        };
        let baseline_tokens = mean(&baseline, |metrics| metrics.model_visible_tokens as f64);
        let candidate_tokens = mean(&candidate, |metrics| metrics.model_visible_tokens as f64);
        let reduction_pct = 100.0 * (1.0 - candidate_tokens / baseline_tokens);
        let baseline_wall_micros = mean(&baseline, |metrics| metrics.wall_micros as f64);
        let candidate_wall_micros = mean(&candidate, |metrics| metrics.wall_micros as f64);
        let summary = json!({
            "benchmark": "controlled-tool-output",
            "repetitions": REPETITIONS,
            "baseline": baseline,
            "candidate": candidate,
            "mean": {
                "baseline_model_visible_tokens": baseline_tokens,
                "candidate_model_visible_tokens": candidate_tokens,
                "token_reduction_pct": reduction_pct,
                "baseline_wall_micros": baseline_wall_micros,
                "candidate_wall_micros": candidate_wall_micros,
                "wall_delta_pct": 100.0 * (candidate_wall_micros / baseline_wall_micros - 1.0),
            },
        });
        println!("CONTROLLED_TOOL_OUTPUT_BENCH={summary}");

        assert!(
            candidate_tokens < baseline_tokens * 0.75,
            "controlled candidate should reduce model-visible tokens by at least 25%: {summary}"
        );
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
        let (notice, newest) = coordination_model_notice(&shared).await.unwrap();
        assert!(notice.contains("1 new unread"));
        assert!(notice.contains("Highest importance: high"));
        assert!(notice.contains("fetch_inbox"));
        assert!(!notice.contains("SUBJECT_SECRET"));
        assert!(!notice.contains("BODY_SECRET"));
        // Candidate repeats until a provider response successfully consumes it.
        assert!(coordination_model_notice(&shared).await.is_some());
        acknowledge_coordination_model_notice(&shared, newest).await;
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
    async fn provider_error_does_not_suppress_coordination_notice_retry() {
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
                crate::coordination::Importance::High,
                false,
                None,
                "2026-07-25T00:00:00Z",
            )
            .unwrap();
        let provider = RecordingProvider::new(vec![
            LlmResponse::error("transient provider failure"),
            end_turn_resp(),
        ]);
        let calls = provider.calls_handle();
        let mut sess = AgentSession::new(Box::new(provider), tool_session, AgentConfig::default());
        let failed = sess.prompt("first").await;
        assert_eq!(failed.stop_reason, StopReason::Error);
        let succeeded = sess.prompt("retry").await;
        assert_eq!(succeeded.stop_reason, StopReason::EndTurn);
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        for (_, _, system) in calls.iter() {
            assert!(system
                .as_deref()
                .unwrap_or_default()
                .contains("DAIMONOS COORDINATION NOTICE"));
        }
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
    async fn agent_bounds_native_tool_result_after_full_content_hook() {
        let dir = tempfile::tempdir().unwrap();
        let original = "large-result-line\n".repeat(200);
        std::fs::write(dir.path().join("large.txt"), &original).unwrap();
        let output_dir = dir.path().join("tool-output");
        let hook_content = Arc::new(Mutex::new(String::new()));
        let captured = Arc::clone(&hook_content);
        let config = AgentConfig {
            after_tool_call: Some(Box::new(move |_, content, _| {
                *captured.lock().unwrap() = content.to_string();
                AfterHookResult::Continue
            })),
            ..AgentConfig::default()
        };
        let provider = Box::new(MockProvider::new(vec![
            tool_call_resp("c1", "read_file", json!({"path": "large.txt"})),
            end_turn_resp(),
        ]));
        let analytics = Arc::new(
            crate::analytics::AnalyticsStore::new(&dir.path().join("analytics.db"), 90).unwrap(),
        );
        let mut session = bounded_session_in(dir.path());
        session.analytics = Some(Arc::clone(&analytics));
        let mut sess = AgentSession::new(provider, session, config);

        sess.prompt("read the large file").await;

        let ContentBlock::ToolResult { content, .. } = &sess.history()[2].content[0] else {
            panic!("expected tool result");
        };
        assert!(hook_content.lock().unwrap().len() > 256);
        assert!(content.len() <= 256);
        assert!(content.contains("full output saved to") || content.contains("full_output_path"));
        let outputs = std::fs::read_dir(&output_dir)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(
            std::fs::read_to_string(outputs[0].path()).unwrap(),
            *hook_content.lock().unwrap()
        );
        assert!(
            !sess
                .tool_session
                .lock()
                .await
                .read_cache
                .contains_key(&dir.path().join("large.txt")),
            "bounded read must not leave a full-visibility cache hit"
        );
        assert!(
            analytics
                .wait_until_quiet(std::time::Duration::from_secs(1))
                .await
        );
        let stats = analytics.session_summary();
        assert!(
            stats
                .per_tool
                .get("context:tool_output")
                .is_some_and(|tool| tool.saved_tokens > 0),
            "agent output offload must record only its additional savings"
        );
    }

    #[tokio::test]
    async fn agent_bounds_meta_tool_results() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(MockProvider::new(vec![
            tool_call_resp("c1", "list_all_tools", json!({})),
            end_turn_resp(),
        ]));
        let mut sess = AgentSession::new(
            provider,
            bounded_session_in(dir.path()),
            AgentConfig::default(),
        );

        sess.prompt("list tools").await;

        let ContentBlock::ToolResult { content, .. } = &sess.history()[2].content[0] else {
            panic!("expected tool result");
        };
        assert!(content.len() <= 256);
        assert!(content.contains("full output saved to") || content.contains("full_output_path"));
        assert_eq!(
            std::fs::read_dir(dir.path().join("tool-output"))
                .unwrap()
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn agent_bounds_execute_script_results() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(MockProvider::new(vec![
            tool_call_resp(
                "c1",
                "execute_script",
                json!({"code": "result = \"x\" * 2000"}),
            ),
            end_turn_resp(),
        ]));
        let mut sess = AgentSession::new(
            provider,
            bounded_session_in(dir.path()),
            AgentConfig::default(),
        );

        sess.prompt("generate output").await;

        let ContentBlock::ToolResult {
            content, is_error, ..
        } = &sess.history()[2].content[0]
        else {
            panic!("expected tool result");
        };
        assert!(!is_error, "{content}");
        assert!(content.len() <= 256);
        assert!(content.contains("full output saved to") || content.contains("full_output_path"));
    }

    #[tokio::test]
    async fn agent_bounds_remote_tool_results() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(MockProvider::new(vec![
            tool_call_resp("c1", "mcp__srv__large", json!({})),
            end_turn_resp(),
        ]));
        let config = AgentConfig {
            remote_tool_dispatch: Some(Box::new(|name: &str, _input: &Value| {
                let handled = name == "mcp__srv__large";
                Box::pin(async move {
                    handled.then(|| RemoteToolResult {
                        content: "remote-output\n".repeat(200),
                        is_error: false,
                    })
                })
            })),
            ..AgentConfig::default()
        };
        let mut sess = AgentSession::new(provider, bounded_session_in(dir.path()), config);

        sess.prompt("call remote").await;

        let ContentBlock::ToolResult { content, .. } = &sess.history()[2].content[0] else {
            panic!("expected tool result");
        };
        assert!(content.len() <= 256);
        assert!(content.contains("full output saved to") || content.contains("full_output_path"));
    }

    /// vikunja #1232: direct tool calls made in agent mode were never written
    /// to analytics. Only `script.rs` (`script:*`) and `mcp_bridge.rs` recorded
    /// per-tool rows, so scripted work was counted while direct work was
    /// invisible — a *biased* gap that made agents look like they always batch
    /// and produced a retracted adoption analysis on #1230.
    #[tokio::test]
    async fn agent_loop_records_direct_tool_calls_in_analytics() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("note.txt"), "hello analytics").unwrap();

        let analytics = Arc::new(
            crate::analytics::AnalyticsStore::new(&dir.path().join("analytics.db"), 90).unwrap(),
        );
        let mut session = session_in(dir.path());
        session.analytics = Some(Arc::clone(&analytics));

        let provider = MockProvider::new(vec![
            tool_call_resp("c1", "read_file", json!({"path": "note.txt"})),
            end_turn_resp(),
        ]);
        let result = run(
            &provider,
            shared(session),
            vec![Message::user("read the note")],
            &AgentConfig {
                tools: vec![ToolSchema {
                    name: "read_file".into(),
                    description: "read".into(),
                    input_schema: json!({"type": "object"}),
                }],
                ..AgentConfig::default()
            },
        )
        .await;
        assert_eq!(result.stop_reason, StopReason::EndTurn);

        assert!(
            analytics
                .wait_until_quiet(std::time::Duration::from_secs(2))
                .await
        );
        let per_tool = analytics.session_summary().per_tool;
        assert!(
            per_tool.contains_key("read_file"),
            "direct read_file call was not recorded; per_tool = {:?}",
            per_tool.keys().collect::<Vec<_>>()
        );
    }

    /// vikunja #1232 companion: instrumenting direct calls must not start
    /// double-counting scripted ones. `script.rs` already records each sandbox
    /// op as `script:*`, and the MCP path returns early for `execute_script`
    /// before `dispatch_tool`, so the agent loop must not add a parent row.
    #[tokio::test]
    async fn agent_loop_does_not_double_count_execute_script() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("note.txt"), "hello analytics").unwrap();

        let analytics = Arc::new(
            crate::analytics::AnalyticsStore::new(&dir.path().join("analytics.db"), 90).unwrap(),
        );
        let mut session = session_in(dir.path());
        session.analytics = Some(Arc::clone(&analytics));

        let provider = MockProvider::new(vec![
            tool_call_resp(
                "c1",
                "execute_script",
                json!({"code": "result = read_file(\"note.txt\")[\"content\"]"}),
            ),
            end_turn_resp(),
        ]);
        let result = run(
            &provider,
            shared(session),
            vec![Message::user("read via script")],
            &AgentConfig::default(),
        )
        .await;
        assert_eq!(result.stop_reason, StopReason::EndTurn);

        assert!(
            analytics
                .wait_until_quiet(std::time::Duration::from_secs(2))
                .await
        );
        let per_tool = analytics.session_summary().per_tool;
        assert!(
            per_tool.contains_key("script:read_file"),
            "sandbox op should still be recorded; per_tool = {:?}",
            per_tool.keys().collect::<Vec<_>>()
        );
        assert!(
            !per_tool.contains_key("execute_script"),
            "execute_script must not get a parent row; per_tool = {:?}",
            per_tool.keys().collect::<Vec<_>>()
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

    /// vikunja #1230: batch adoption is measured by how many tool ops a single
    /// script dispatched, not by how many bytes the script was.
    ///
    /// Script *size* was a proxy for "did real multi-op work happen", and a bad
    /// one: the verified-optimal solution to bench task 03 (read, string
    /// replace, write, cargo test) serializes to ~250-550 bytes and scored zero
    /// against the 700-byte threshold, while a forced pilot that produced a
    /// 1701-byte script scored as adoption despite erroring twice. The proxy
    /// rewarded verbosity; ops-per-script measures the lever directly.
    ///
    /// `op_count` is already tracked (`agent.rs` execute_script branch, feeding
    /// `ToolOutcome::batch_size` per ADR-006 D5) — it just never reached the
    /// token log the benchmark reads.
    #[test]
    fn token_log_line_reports_script_ops_for_batch_adoption() {
        let composition = crate::context_metrics::ContextComposition::default();
        let metadata = GenerationLogMetadata {
            kind: "agent",
            ordinal: 2,
            stop_reason: &StopReason::ToolUse,
            response_tool_calls: 1,
            context: &composition,
            script_ops_total: 5,
            script_ops_max: 3,
        };

        let line = token_log_line("agent", "m", &Usage::default(), Some(&metadata));
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();

        assert_eq!(parsed["script_ops_total"], 5);
        assert_eq!(parsed["script_ops_max"], 3);
    }

    /// End-to-end: a real script dispatching two ops must show up as adoption in
    /// the token log the benchmark reads (#1230). The unit tests above only pin
    /// the log shape and the rule; this proves the counter is actually wired to
    /// `op_count` in the dispatch path.
    #[tokio::test]
    async fn run_records_script_ops_from_a_real_multi_op_script() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha").unwrap();
        std::fs::write(dir.path().join("b.txt"), "beta").unwrap();
        let log = dir.path().join("tokens.jsonl");

        // Two dispatched ops inside one script — the batching case.
        let code = r#"result = read_file("a.txt")["content"] + read_file("b.txt")["content"]"#;
        let provider = MockProvider::new(vec![
            tool_call_resp("c1", "execute_script", json!({"code": code})),
            end_turn_resp(),
        ]);
        let config = AgentConfig {
            token_log: Some(TokenLogConfig {
                path: log.clone(),
                label: "agent".into(),
            }),
            ..AgentConfig::default()
        };

        let result = run(
            &provider,
            shared(session_in(dir.path())),
            vec![Message::user("concat")],
            &config,
        )
        .await;
        assert_eq!(result.stop_reason, StopReason::EndTurn);

        let lines: Vec<serde_json::Value> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let last = lines.last().expect("at least one token log line");

        assert_eq!(last["script_ops_max"], 2, "two reads in one script");
        assert_eq!(last["script_ops_total"], 2);
        assert!(is_batch_adoption(
            last["script_ops_max"].as_u64().unwrap() as usize
        ));
    }

    /// The discriminating case, and the reason the byte threshold was wrong: a
    /// script that dispatches a single op is not batching. This is exactly what
    /// the #1230 B1 arm produced — forced to use `execute_script` for its first
    /// generation, the model emitted a 71-byte script that did one read, then
    /// reverted to sequential tool calls once the full catalog returned.
    #[tokio::test]
    async fn a_single_op_script_is_not_batch_adoption() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha").unwrap();
        let log = dir.path().join("tokens.jsonl");

        let provider = MockProvider::new(vec![
            tool_call_resp(
                "c1",
                "execute_script",
                json!({"code": r#"result = read_file("a.txt")["content"]"#}),
            ),
            end_turn_resp(),
        ]);
        let config = AgentConfig {
            token_log: Some(TokenLogConfig {
                path: log.clone(),
                label: "agent".into(),
            }),
            ..AgentConfig::default()
        };
        run(
            &provider,
            shared(session_in(dir.path())),
            vec![Message::user("peek")],
            &config,
        )
        .await;

        let last: serde_json::Value = serde_json::from_str(
            std::fs::read_to_string(&log)
                .unwrap()
                .lines()
                .rfind(|l| !l.trim().is_empty())
                .unwrap(),
        )
        .unwrap();

        assert_eq!(last["script_ops_max"], 1);
        assert!(
            !is_batch_adoption(last["script_ops_max"].as_u64().unwrap() as usize),
            "a one-op script is a single tool call in a costume, not a batch"
        );
    }

    /// The classification rule itself. A 1-op script is *not* batch adoption —
    /// it is a single tool call in a costume, which is exactly what the #1230
    /// B1 arm produced (a 71-byte inspection script) and what the byte
    /// threshold could not distinguish from a real batch.
    #[test]
    fn batch_adoption_requires_a_script_that_dispatched_multiple_ops() {
        assert!(!is_batch_adoption(0), "no script at all");
        assert!(!is_batch_adoption(1), "single op wrapped in a script");
        assert!(
            is_batch_adoption(2),
            "two ops collapsed into one round-trip"
        );
        assert!(is_batch_adoption(9));
    }

    #[test]
    fn token_log_line_has_expected_fields() {
        let usage = Usage {
            input: 120,
            output: 45,
            reasoning_output: 11,
            cache_read: 3,
            cache_write: 7,
            cost: Cost {
                total_usd: 0.0012,
                ..Cost::default()
            },
        };
        let composition = crate::context_metrics::ContextComposition {
            messages: 3,
            tools_exposed: 2,
            system_bytes: 100,
            tool_result_ok_bytes: 200,
            payload_bytes: 300,
            payload_tokens_est: 75,
            ..crate::context_metrics::ContextComposition::default()
        };
        let metadata = GenerationLogMetadata {
            kind: "agent",
            ordinal: 4,
            stop_reason: &StopReason::ToolUse,
            response_tool_calls: 2,
            context: &composition,
            script_ops_total: 0,
            script_ops_max: 0,
        };
        let line = token_log_line("chat", "claude-haiku-4-5", &usage, Some(&metadata));
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["cmd"], "chat");
        assert_eq!(parsed["model"], "claude-haiku-4-5");
        assert_eq!(parsed["input"], 120);
        assert_eq!(parsed["output"], 45);
        assert_eq!(parsed["reasoning_output"], 11);
        assert_eq!(parsed["cache_read"], 3);
        assert_eq!(parsed["cache_write"], 7);
        assert_eq!(parsed["cost_usd"], "0.001200");
        assert_eq!(parsed["schema_version"], 2);
        assert_eq!(parsed["generation_kind"], "agent");
        assert_eq!(parsed["ordinal"], 4);
        assert_eq!(parsed["stop_reason"], "tool_use");
        assert_eq!(parsed["response_tool_calls"], 2);
        assert_eq!(parsed["context"]["messages"], 3);
        assert_eq!(parsed["context"]["tool_result_ok_bytes"], 200);
        assert_eq!(parsed["context"]["payload_tokens_est"], 75);
        assert!(parsed["ts"].is_string(), "must include a timestamp");
    }

    #[test]
    fn log_token_usage_appends_one_line_per_call() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = TokenLogConfig {
            path: dir.path().join("tokens.log"),
            label: "agent".to_string(),
        };
        log_token_usage(&cfg, "m1", &mock_usage(10, 5), None);
        log_token_usage(&cfg, "m1", &mock_usage(20, 8), None);
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
        log_token_usage(&cfg, "m1", &mock_usage(1, 1), None);
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
        let entry: Value = serde_json::from_str(content.lines().next().unwrap()).unwrap();
        assert_eq!(entry["cmd"], "agent");
        assert_eq!(entry["generation_kind"], "agent");
        assert_eq!(entry["stop_reason"], "end_turn");
        assert_eq!(entry["ordinal"], 0);
        assert_eq!(entry["context"]["messages"], 1);
        assert!(entry["context"]["payload_tokens_est"].as_u64().unwrap() > 0);
    }

    /// vikunja 1112. The catalog handed to the model advertised 19 tools the
    /// agent loop could not dispatch, so each one failed with "not available in
    /// agent mode". This drives `run` end-to-end — asserting on the dispatcher in
    /// isolation is not enough, because the defect was the loop not *calling* it.
    #[tokio::test]
    async fn agent_loop_dispatches_plugin_and_meta_tools() {
        // One plugin tool and one meta tool: the two families that were dead.
        for tool in ["shellcheck", "list_all_tools"] {
            let dir = tempfile::tempdir().unwrap();
            let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let hook_seen = Arc::clone(&seen);
            let config = AgentConfig {
                after_tool_call: Some(Box::new(move |_info, content, _is_error| {
                    hook_seen.lock().unwrap().push(content.to_string());
                    AfterHookResult::Continue
                })),
                ..AgentConfig::default()
            };
            let provider = MockProvider::new(vec![
                tool_call_resp("call-1", tool, json!({})),
                end_turn_resp(),
            ]);

            run(
                &provider,
                shared(session_in(dir.path())),
                vec![Message::user("go")],
                &config,
            )
            .await;

            let results = seen.lock().unwrap().clone();
            assert_eq!(results.len(), 1, "{tool}: expected exactly one tool result");
            assert!(
                !results[0].contains("not available in agent mode"),
                "{tool} was advertised but not dispatchable by the agent loop: {}",
                results[0]
            );
        }
    }

    #[tokio::test]
    async fn agent_list_all_tools_includes_remote_catalog_entries() {
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(String::new()));
        let captured = Arc::clone(&seen);
        let config = AgentConfig {
            tools: vec![ToolSchema {
                name: "mcp__linear__get_issue".into(),
                description: "Get a Linear issue".into(),
                input_schema: json!({"type":"object"}),
            }],
            after_tool_call: Some(Box::new(move |_, content, _| {
                *captured.lock().unwrap() = content.to_string();
                AfterHookResult::Continue
            })),
            ..AgentConfig::default()
        };
        let provider = MockProvider::new(vec![
            tool_call_resp("call-1", "list_all_tools", json!({})),
            end_turn_resp(),
        ]);

        run(
            &provider,
            shared(session_in(dir.path())),
            vec![Message::user("list tools")],
            &config,
        )
        .await;

        let entries: Vec<Value> = serde_json::from_str(&seen.lock().unwrap()).unwrap();
        assert!(entries
            .iter()
            .any(|entry| entry["name"] == "mcp__linear__get_issue"));
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

    /// vikunja #1284: `list_all_tools` is served from the *MCP* catalog
    /// (`tool_definitions`, i.e. everything `defined_for_mcp`), but the agent's
    /// own tool list is `exposed_to_agent`. Reporting the difference tells the
    /// model about tools it cannot call — `batch` in particular is intercepted
    /// in the MCP request handler and has no agent-side implementation at all,
    /// so it fails on every call. That is the #1112 defect arriving through the
    /// catalog instead of the schema list.
    #[test]
    fn list_all_tools_catalog_omits_tools_the_agent_cannot_call() {
        let native = serde_json::to_string(
            &crate::tools::all_tools()
                .iter()
                .filter(|t| t.tier.defined_for_mcp())
                .map(|t| serde_json::json!({"name": t.name, "description": "d"}))
                .collect::<Vec<_>>(),
        )
        .unwrap();

        let catalog = append_remote_tools_to_catalog(native, &[]);
        let entries: Vec<Value> = serde_json::from_str(&catalog).unwrap();
        let listed: Vec<&str> = entries.iter().filter_map(|e| e["name"].as_str()).collect();

        let unreachable: Vec<&str> = crate::tools::all_tools()
            .iter()
            .filter(|t| !t.tier.exposed_to_agent())
            .map(|t| t.name)
            .collect();
        assert!(
            !unreachable.is_empty(),
            "no agent-unreachable tiers left; this test is now vacuous"
        );

        for name in unreachable {
            assert!(
                !listed.contains(&name),
                "{name} is not exposed to the agent but was listed in its catalog"
            );
        }
        // The reachable ones must survive the filter.
        assert!(listed.contains(&"read_file"));
        assert!(listed.contains(&"execute_script"));
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
                retryable: false,
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
    async fn microcompaction_invalidates_evicted_read_cache_entry() {
        let dir = tempfile::tempdir().unwrap();
        let cached_path = dir.path().join("cached.txt");
        std::fs::write(&cached_path, "cached").unwrap();
        let mut cfg = Config::default();
        cfg.tool_output.directory =
            Some(dir.path().join("tool-output").to_string_lossy().to_string());
        cfg.tool_output.intra_turn_result_budget_tokens = 1;
        cfg.tool_output.intra_turn_keep_recent_results = 1;
        let mut session = Session::new(dir.path().to_path_buf(), Arc::new(cfg));
        session.read_cache.insert(
            cached_path.clone(),
            crate::session::ReadCacheEntry { hash: 1, lines: 1 },
        );
        session.read_cache.insert(
            dir.path().join("unrelated.txt"),
            crate::session::ReadCacheEntry { hash: 2, lines: 1 },
        );
        let session = shared(session);
        let mut messages = vec![
            Message::user("task"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolCall {
                    id: "read".into(),
                    name: "read_file".into(),
                    input: json!({"path":"cached.txt"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "read".into(),
                    content: "old read result".repeat(100),
                    is_error: false,
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolCall {
                    id: "recent".into(),
                    name: "search".into(),
                    input: json!({"pattern":"x"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "recent".into(),
                    content: "recent".into(),
                    is_error: false,
                }],
            },
        ];

        microcompact_agent_history(&mut messages, &session).await;

        assert!(
            session.lock().await.read_cache.is_empty(),
            "relative historical read paths are cwd-ambiguous, so the cache must be cleared"
        );
    }

    #[tokio::test]
    async fn microcompaction_clears_read_cache_when_evicted_path_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let cached_path = dir.path().join("other.txt");
        let mut cfg = Config::default();
        cfg.tool_output.directory =
            Some(dir.path().join("tool-output").to_string_lossy().to_string());
        cfg.tool_output.intra_turn_result_budget_tokens = 1;
        cfg.tool_output.intra_turn_keep_recent_results = 1;
        let mut session = Session::new(dir.path().to_path_buf(), Arc::new(cfg));
        session.read_cache.insert(
            cached_path,
            crate::session::ReadCacheEntry { hash: 1, lines: 1 },
        );
        let session = shared(session);
        let mut messages = vec![
            Message::user("task"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolCall {
                    id: "read".into(),
                    name: "read_file".into(),
                    input: json!({}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "read".into(),
                    content: "old read result".repeat(100),
                    is_error: false,
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolCall {
                    id: "recent".into(),
                    name: "search".into(),
                    input: json!({"pattern":"x"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "recent".into(),
                    content: "recent".into(),
                    is_error: false,
                }],
            },
        ];

        microcompact_agent_history(&mut messages, &session).await;

        assert!(session.lock().await.read_cache.is_empty());
    }

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
            retryable: false,
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

    fn max_tokens_text_resp() -> LlmResponse {
        LlmResponse {
            retryable: false,
            content: vec![ContentBlock::Text("partial output".to_string())],
            stop_reason: StopReason::MaxTokens,
            error_message: None,
            context_overflow: false,
            usage: mock_usage(100, 50),
        }
    }

    struct ThinkingCaptureProvider {
        responses: Mutex<VecDeque<LlmResponse>>,
        thinking: Arc<Mutex<Vec<ThinkingLevel>>>,
    }

    impl ThinkingCaptureProvider {
        fn new(responses: Vec<LlmResponse>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(responses)),
                thinking: Arc::new(Mutex::new(Vec::new())),
            }
        }
        fn thinking_handle(&self) -> Arc<Mutex<Vec<ThinkingLevel>>> {
            Arc::clone(&self.thinking)
        }
    }

    #[async_trait]
    impl LlmProvider for ThinkingCaptureProvider {
        async fn complete(&self, _ctx: &Context, opts: &CompleteOpts) -> LlmResponse {
            self.thinking.lock().unwrap().push(opts.thinking.clone());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| LlmResponse::error("ThinkingCaptureProvider exhausted"))
        }
    }

    #[tokio::test]
    async fn max_tokens_auto_continue_completes_in_one_turn() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        let provider = MockProvider::new(vec![max_tokens_text_resp(), end_turn_resp()]);
        let config = AgentConfig {
            auto_continue_budget: Some(3),
            ..AgentConfig::default()
        };
        let result = run(&provider, shared(s), vec![Message::user("go")], &config).await;
        // A plain-text truncation is auto-continued into a single logical turn
        // that ends cleanly instead of dead-stopping to the client.
        assert_eq!(result.stop_reason, StopReason::EndTurn);
    }

    #[tokio::test]
    async fn auto_continue_disabled_returns_terminal_max_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        let provider = MockProvider::new(vec![max_tokens_text_resp()]);
        let config = AgentConfig {
            auto_continue_budget: Some(0),
            ..AgentConfig::default()
        };
        let result = run(&provider, shared(s), vec![Message::user("go")], &config).await;
        // Budget 0 preserves the historical behavior: terminal MaxTokens.
        assert_eq!(result.stop_reason, StopReason::MaxTokens);
    }

    #[tokio::test]
    async fn auto_continue_is_bounded_by_budget() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        let provider = BenchmarkCaptureProvider::new(vec![
            max_tokens_text_resp(),
            max_tokens_text_resp(),
            end_turn_resp(),
        ]);
        let contexts = provider.contexts_handle();
        let config = AgentConfig {
            auto_continue_budget: Some(1),
            ..AgentConfig::default()
        };
        let result = run(&provider, shared(s), vec![Message::user("go")], &config).await;
        // Budget 1: exactly one continuation, then the second MaxTokens exhausts
        // the budget and returns terminal MaxTokens — the queued EndTurn (a
        // third call) is never reached.
        assert_eq!(result.stop_reason, StopReason::MaxTokens);
        assert_eq!(contexts.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn text_truncation_auto_continue_inserts_user_turn_between_assistants() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        let provider = BenchmarkCaptureProvider::new(vec![max_tokens_text_resp(), end_turn_resp()]);
        let contexts = provider.contexts_handle();
        let config = AgentConfig {
            auto_continue_budget: Some(2),
            ..AgentConfig::default()
        };
        let result = run(&provider, shared(s), vec![Message::user("go")], &config).await;
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        let ctxs = contexts.lock().unwrap();
        // The continuation (2nd) call must not present two consecutive assistant
        // messages: a user turn is inserted after the truncated assistant turn.
        let second = &ctxs[1];
        assert!(matches!(second.last().unwrap().role, Role::User));
        for pair in second.windows(2) {
            assert!(
                !(matches!(pair[0].role, Role::Assistant)
                    && matches!(pair[1].role, Role::Assistant)),
                "consecutive assistant messages must not reach the provider"
            );
        }
    }

    #[tokio::test]
    async fn auto_continue_forces_thinking_off_on_continuation() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        let provider = ThinkingCaptureProvider::new(vec![max_tokens_text_resp(), end_turn_resp()]);
        let thinking = provider.thinking_handle();
        let config = AgentConfig {
            auto_continue_budget: Some(2),
            ..AgentConfig::default()
        };
        let result = run(&provider, shared(s), vec![Message::user("go")], &config).await;
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        let seen = thinking.lock().unwrap();
        assert_eq!(seen.len(), 2, "one continuation after the truncation");
        assert!(
            !matches!(seen[0], ThinkingLevel::Off),
            "first call keeps the caller's thinking level"
        );
        assert!(
            matches!(seen[1], ThinkingLevel::Off),
            "continuation forces thinking off so reasoning can't re-consume the budget"
        );
    }

    #[tokio::test]
    async fn max_tokens_truncated_tool_call_auto_continues_without_executing() {
        let dir = tempfile::tempdir().unwrap();
        let provider = BenchmarkCaptureProvider::new(vec![
            LlmResponse {
                retryable: false,
                content: vec![ContentBlock::ToolCall {
                    id: "cut-off-call".into(),
                    name: "write_file".into(),
                    input: json!({
                        "path": "must-not-exist.txt",
                        "content": "must not be written"
                    }),
                }],
                stop_reason: StopReason::MaxTokens,
                error_message: None,
                context_overflow: false,
                usage: Usage::default(),
            },
            end_turn_resp(),
        ]);
        let config = AgentConfig {
            auto_continue_budget: Some(2),
            ..AgentConfig::default()
        };
        let result = run(
            &provider,
            shared(session_in(dir.path())),
            vec![Message::user("go")],
            &config,
        )
        .await;
        // The truncated tool call is closed as interrupted (not executed), then
        // the turn auto-continues to a clean end.
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert!(!dir.path().join("must-not-exist.txt").exists());
        assert!(orphan_ids(&result.messages).is_empty());
    }

    #[tokio::test]
    async fn max_tokens_tool_call_is_closed_without_execution() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        let provider = MockProvider::new(vec![LlmResponse {
            retryable: false,
            content: vec![ContentBlock::ToolCall {
                id: "cut-off-call".into(),
                name: "write_file".into(),
                input: json!({
                    "path": "must-not-exist.txt",
                    "content": "must not be written"
                }),
            }],
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
        assert!(orphan_ids(&result.messages).is_empty());
        assert!(matches!(
            result.messages[2].content.as_slice(),
            [ContentBlock::ToolResult {
                tool_use_id,
                is_error: true,
                content,
            }] if tool_use_id == "cut-off-call" && content == INTERRUPTED_TOOL_RESULT
        ));
        assert!(!dir.path().join("must-not-exist.txt").exists());
    }

    #[tokio::test]
    async fn same_session_continues_after_max_tokens_tool_call() {
        let dir = tempfile::tempdir().unwrap();
        let provider = BenchmarkCaptureProvider::new(vec![
            LlmResponse {
                retryable: false,
                content: vec![ContentBlock::ToolCall {
                    id: "cut-off-call".into(),
                    name: "write_file".into(),
                    input: json!({}),
                }],
                stop_reason: StopReason::MaxTokens,
                error_message: None,
                context_overflow: false,
                usage: Usage::default(),
            },
            end_turn_resp(),
        ]);
        let contexts = provider.contexts_handle();
        let mut session = AgentSession::new(
            Box::new(provider),
            session_in(dir.path()),
            AgentConfig::default(),
        );

        let first = session.prompt("start").await;
        let second = session.prompt("continue").await;

        assert_eq!(first.stop_reason, StopReason::MaxTokens);
        assert_eq!(second.stop_reason, StopReason::EndTurn);
        assert!(orphan_ids(session.history()).is_empty());
        let contexts = contexts.lock().unwrap();
        assert_eq!(contexts.len(), 2);
        assert!(orphan_ids(&contexts[1]).is_empty());
    }

    #[tokio::test]
    async fn usage_accumulates_across_turns() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        let provider = MockProvider::new(vec![
            tool_call_resp("t1", "nonexistent_tool", json!({})),
            LlmResponse {
                retryable: false,
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
            retryable: false,
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
            retryable: false,
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

    // --- orphan tool-call repair ---

    fn tool_call(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolCall {
                id: id.to_string(),
                name: "edit_file".to_string(),
                input: json!({}),
            }],
        }
    }

    fn tool_result(id: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: "ok".to_string(),
                is_error: false,
            }],
        }
    }

    fn orphan_ids(messages: &[Message]) -> Vec<&str> {
        let answered: HashSet<&str> = messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::ToolCall { id, .. } if !answered.contains(id.as_str()) => {
                    Some(id.as_str())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn close_orphan_tool_calls_leaves_paired_history_untouched() {
        let history = vec![
            Message::user("go"),
            tool_call("c1"),
            tool_result("c1"),
            Message::assistant("done"),
        ];
        assert!(!has_orphan_tool_calls(&history));
        assert_eq!(
            close_orphan_tool_calls(history.clone()).len(),
            history.len()
        );
        assert!(orphan_ids(&close_orphan_tool_calls(history)).is_empty());
    }

    #[test]
    fn close_orphan_tool_calls_pairs_a_turn_cut_off_mid_call() {
        // The wedged-session shape: a call persisted with no result, then the
        // user prompting again. Every later prompt 400s until this is closed.
        let history = vec![
            Message::user("go"),
            tool_call("c1"),
            Message::user("pick up where you left off"),
        ];
        assert!(has_orphan_tool_calls(&history));
        let repaired = close_orphan_tool_calls(history);

        assert!(orphan_ids(&repaired).is_empty());
        // Inserted directly after the call, before the next user text.
        assert!(matches!(
            repaired[2].content.as_slice(),
            [ContentBlock::ToolResult { tool_use_id, is_error: true, .. }] if tool_use_id == "c1"
        ));
        assert!(
            matches!(repaired[3].content.as_slice(), [ContentBlock::Text(t)] if t == "pick up where you left off")
        );
    }

    #[test]
    fn close_orphan_tool_calls_closes_only_the_unanswered_parallel_call() {
        let repaired = close_orphan_tool_calls(vec![
            Message::user("go"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::ToolCall {
                        id: "c1".to_string(),
                        name: "exec".to_string(),
                        input: json!({}),
                    },
                    ContentBlock::ToolCall {
                        id: "c2".to_string(),
                        name: "exec".to_string(),
                        input: json!({}),
                    },
                ],
            },
            tool_result("c1"),
        ]);

        assert!(orphan_ids(&repaired).is_empty());
        assert!(matches!(
            repaired[2].content.as_slice(),
            [ContentBlock::ToolResult { tool_use_id, .. }] if tool_use_id == "c2"
        ));
    }

    #[test]
    fn set_history_repairs_orphans_without_shifting_user_turns() {
        let dir = tempfile::tempdir().unwrap();
        let mut sess = AgentSession::new(
            Box::new(MockProvider::new(vec![])),
            session_in(dir.path()),
            AgentConfig::default(),
        );
        sess.set_history(vec![
            Message::user("go"),
            tool_call("c1"),
            Message::user("pick up where you left off"),
        ]);

        assert!(orphan_ids(sess.history()).is_empty());
        // Synthetic ToolResult messages are not user turns, so the
        // client_user_message_ids alignment in acp_cmd stays correct.
        assert_eq!(sess.user_turn_count(), 2);
    }

    #[tokio::test]
    async fn repaired_history_is_accepted_by_the_next_prompt_context() {
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut sess = AgentSession::new(
            Box::new(CaptureProvider {
                seen: Arc::clone(&seen),
            }),
            session_in(dir.path()),
            AgentConfig::default(),
        );
        sess.set_history(vec![Message::user("go"), tool_call("c1")]);

        let turn = sess.prompt("continue").await;
        assert_eq!(turn.stop_reason, StopReason::EndTurn);
        let provider_context = seen.lock().unwrap().clone();
        assert!(orphan_ids(&provider_context).is_empty());
        assert!(provider_context.iter().any(|message| {
            matches!(
                message.content.as_slice(),
                [ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error: true,
                }] if tool_use_id == "c1" && content == INTERRUPTED_TOOL_RESULT
            )
        }));
    }

    #[tokio::test]
    async fn prompt_boundary_repairs_live_in_memory_orphan() {
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut sess = AgentSession::new(
            Box::new(CaptureProvider {
                seen: Arc::clone(&seen),
            }),
            session_in(dir.path()),
            AgentConfig::default(),
        );
        // Simulate a live frontend session that was committed before terminal
        // response repair existed. No reload/set_history occurs in this path.
        sess.messages = vec![Message::user("go"), tool_call("c1")];

        let turn = sess.prompt("continue").await;

        assert_eq!(turn.stop_reason, StopReason::EndTurn);
        let provider_context = seen.lock().unwrap().clone();
        assert!(orphan_ids(&provider_context).is_empty());
        assert!(provider_context.iter().any(|message| {
            matches!(
                message.content.as_slice(),
                [ContentBlock::ToolResult {
                    tool_use_id,
                    is_error: true,
                    ..
                }] if tool_use_id == "c1"
            )
        }));
    }
}
