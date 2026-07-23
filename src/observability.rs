use std::collections::HashMap;
use std::time::Duration;

use base64::Engine;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{
    BatchConfigBuilder, BatchSpanProcessor, Sampler, SdkTracer, SdkTracerProvider, SpanData,
    SpanExporter,
};
use opentelemetry_sdk::Resource;
use sha2::{Digest, Sha256};

use crate::config::ObservabilityConfig;

pub const TRACE_TARGET: &str = "daimonos::observability";
pub const LOCAL_DIAGNOSTIC_TARGET: &str = "daimonos::observability_local";

pub struct PromptMetadata<'a> {
    pub mode: &'a str,
    pub session_id: Option<&'a str>,
    pub model: &'a str,
    pub workspace: &'a std::path::Path,
    pub turn_index: usize,
    pub tools_exposed: usize,
}

pub struct PromptSpan {
    span: tracing::Span,
    started: std::time::Instant,
}

pub struct GenerationMetadata<'a> {
    pub kind: &'a str,
    pub model: &'a str,
    pub max_tokens: u32,
    pub thinking: crate::providers::ThinkingLevel,
    pub temperature: Option<f64>,
    pub ordinal: u64,
    pub tools_exposed: usize,
    pub stable_prefix_len: usize,
}

pub struct GenerationSpan {
    span: tracing::Span,
    started: std::time::Instant,
    first_token_recorded: std::sync::atomic::AtomicBool,
}

impl GenerationSpan {
    pub fn new(metadata: GenerationMetadata<'_>) -> Self {
        let mut model_parameters = serde_json::json!({
            "max_tokens": metadata.max_tokens,
            "thinking": metadata.thinking.as_str(),
        });
        if let Some(temperature) = metadata.temperature {
            model_parameters["temperature"] = serde_json::json!(temperature);
        }
        let model_parameters = model_parameters.to_string();
        let span = tracing::info_span!(
            target: TRACE_TARGET,
            "llm.generation",
            otel.name = "llm.generation",
            otel.kind = "client",
            "langfuse.observation.type" = "generation",
            "langfuse.observation.model.name" = metadata.model,
            "langfuse.observation.model.parameters" = model_parameters,
            "gen_ai.request.model" = metadata.model,
            "gen_ai.request.max_tokens" = metadata.max_tokens as u64,
            "gen_ai.request.temperature" = tracing::field::Empty,
            "daimonos.thinking.level" = metadata.thinking.as_str(),
            "daimonos.generation.kind" = metadata.kind,
            "daimonos.generation.ordinal" = metadata.ordinal,
            "daimonos.tools.exposed" = metadata.tools_exposed as u64,
            "daimonos.stable_prefix_len" = metadata.stable_prefix_len as u64,
            "daimonos.time_to_first_token_ms" = tracing::field::Empty,
            "langfuse.observation.completion_start_time" = tracing::field::Empty,
            "gen_ai.usage.input_tokens" = tracing::field::Empty,
            "gen_ai.usage.output_tokens" = tracing::field::Empty,
            "daimonos.usage.cache_read" = tracing::field::Empty,
            "daimonos.usage.cache_write" = tracing::field::Empty,
            "langfuse.observation.usage_details" = tracing::field::Empty,
            "langfuse.observation.cost_details" = tracing::field::Empty,
            "gen_ai.response.finish_reasons" = tracing::field::Empty,
            "daimonos.context_overflow" = tracing::field::Empty,
            "daimonos.duration_ms" = tracing::field::Empty,
            "error.type" = tracing::field::Empty,
        );
        if let Some(temperature) = metadata.temperature {
            span.record("gen_ai.request.temperature", temperature);
        }
        Self {
            span,
            started: std::time::Instant::now(),
            first_token_recorded: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn span(&self) -> &tracing::Span {
        &self.span
    }

    pub fn mark_first_token(&self) {
        use std::sync::atomic::Ordering;

        if !self.first_token_recorded.swap(true, Ordering::Relaxed) {
            self.span.record(
                "daimonos.time_to_first_token_ms",
                self.started.elapsed().as_millis() as u64,
            );
            self.span.record(
                "langfuse.observation.completion_start_time",
                chrono::Utc::now().to_rfc3339(),
            );
        }
    }

    pub fn finish(self, response: &crate::providers::LlmResponse) {
        let usage = &response.usage;
        self.span.record("gen_ai.usage.input_tokens", usage.input);
        self.span.record("gen_ai.usage.output_tokens", usage.output);
        self.span
            .record("daimonos.usage.cache_read", usage.cache_read);
        self.span
            .record("daimonos.usage.cache_write", usage.cache_write);
        self.span.record(
            "langfuse.observation.usage_details",
            serde_json::json!({
                "input": usage.input,
                "output": usage.output,
                "cache_read": usage.cache_read,
                "cache_write": usage.cache_write,
            })
            .to_string(),
        );
        self.span.record(
            "langfuse.observation.cost_details",
            serde_json::json!({
                "input_usd": usage.cost.input_usd,
                "output_usd": usage.cost.output_usd,
                "cache_read_usd": usage.cost.cache_read_usd,
                "cache_write_usd": usage.cost.cache_write_usd,
                "total_usd": usage.cost.total_usd,
            })
            .to_string(),
        );
        self.span.record(
            "gen_ai.response.finish_reasons",
            serde_json::json!([response.stop_reason.as_str()]).to_string(),
        );
        self.span
            .record("daimonos.context_overflow", response.context_overflow);
        self.span.record(
            "daimonos.duration_ms",
            self.started.elapsed().as_millis() as u64,
        );
        let error_type = match response.stop_reason {
            crate::providers::StopReason::Error => Some("provider_error"),
            crate::providers::StopReason::Refusal => Some("refusal"),
            _ => None,
        };
        if let Some(error_type) = error_type {
            self.span.record("error.type", error_type);
        }
    }
}

impl PromptSpan {
    pub fn new(metadata: PromptMetadata<'_>) -> Self {
        let workspace_id = workspace_id(metadata.workspace);
        let span = tracing::info_span!(
            target: TRACE_TARGET,
            parent: None,
            "agent.prompt",
            otel.name = "agent.prompt",
            otel.kind = "internal",
            "langfuse.trace.name" = "agent.prompt",
            "langfuse.session.id" = tracing::field::Empty,
            "daimonos.runtime.mode" = metadata.mode,
            "daimonos.workspace.id" = workspace_id,
            "daimonos.turn.index" = metadata.turn_index as u64,
            "daimonos.tools.exposed" = metadata.tools_exposed as u64,
            "gen_ai.request.model" = metadata.model,
            "daimonos.stop_reason" = tracing::field::Empty,
            "daimonos.cancel.reason" = tracing::field::Empty,
            "daimonos.duration_ms" = tracing::field::Empty,
            "error.type" = tracing::field::Empty,
        );
        if let Some(session_id) = metadata.session_id {
            span.record("langfuse.session.id", session_id);
        }
        Self {
            span,
            started: std::time::Instant::now(),
        }
    }

    pub fn span(&self) -> &tracing::Span {
        &self.span
    }

    /// Record the normalized cancellation cause (`client`, `transport`,
    /// `timeout`, or `policy`) on the active prompt root (ADR-006 D5). Called
    /// by the frontend that detects the cancellation before `finish`.
    pub fn record_cancel_reason(&self, reason: &str) {
        self.span.record("daimonos.cancel.reason", reason);
    }

    pub fn finish(self, stop_reason: &str, error_type: Option<&str>) {
        self.span.record("daimonos.stop_reason", stop_reason);
        self.span.record(
            "daimonos.duration_ms",
            self.started.elapsed().as_millis() as u64,
        );
        if let Some(error_type) = error_type {
            self.span.record("error.type", error_type);
        }
    }
}

/// Static `daimonos.tool.kind` classification by tool name.
///
/// `native` = opcode/file/exec/search tools dispatched through the opcode
/// facade; `plugin` = `ToolRegistry` plugin tools (git/cargo/…) that run as
/// first-class tools on the MCP-server path; `script` = the `execute_script`
/// Starlark runtime. Remote MCP tools are classified as `remote` at dispatch,
/// not here.
///
/// The plugin set mirrors the `ToolRegistry` plugins in `tools.rs`; keep it in
/// sync when adding/renaming a plugin tool, or it falls back to `native`. The
/// label is cosmetic (a misclassification never affects dispatch), so an exact
/// compile-time link is deliberately not enforced.
pub fn tool_kind(name: &str) -> &'static str {
    match name {
        "execute_script" => "script",
        "git" | "cargo" | "gh" | "docker" | "pytest" | "npm" | "curl" | "shellcheck"
        | "discord" => "plugin",
        _ => "native",
    }
}

/// Forwarded-server alias for a namespaced remote tool name of the documented
/// `mcp__{server}__{tool}` form (ADR-003). Returns `None` for names that don't
/// follow the scheme. Collision suffixes attach to the tool segment, so the
/// server segment is recovered reliably and stays low-cardinality (D8).
pub fn remote_server_alias(name: &str) -> Option<&str> {
    name.strip_prefix(crate::mcp_bridge::REMOTE_TOOL_PREFIX)
        .and_then(|rest| rest.split_once("__"))
        .map(|(server, _tool)| server)
}

/// Normalized terminal state of one tool call, kept low-cardinality for D5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Success,
    Error,
    Blocked,
    Timeout,
    Unavailable,
}

impl ToolStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolStatus::Success => "success",
            ToolStatus::Error => "error",
            ToolStatus::Blocked => "blocked",
            ToolStatus::Timeout => "timeout",
            ToolStatus::Unavailable => "unavailable",
        }
    }

    /// The bounded `error.type` class for a non-success status, or `None` when
    /// the call succeeded. Never carries a provider or message string (D6).
    fn error_type(self) -> Option<&'static str> {
        match self {
            ToolStatus::Success => None,
            ToolStatus::Error => Some("tool_error"),
            ToolStatus::Blocked => Some("blocked"),
            ToolStatus::Timeout => Some("timeout"),
            ToolStatus::Unavailable => Some("unavailable"),
        }
    }
}

/// Metadata-only outcome of a tool call. Sizes are coarse token estimates; no
/// argument, command, or result bodies are ever included (D6).
#[derive(Debug, Default, Clone, Copy)]
pub struct ToolOutcome {
    pub request_tokens_est: u64,
    pub response_tokens_est: u64,
    pub saved_tokens_est: i64,
    pub redirect: bool,
    pub filtered: bool,
    pub read_dedup: bool,
    pub batch_size: u64,
}

/// A `tool.call` span for a native/plugin/script tool executed under a prompt
/// root. Created as a child of the current span; closed via [`ToolSpan::finish`].
pub struct ToolSpan {
    span: tracing::Span,
    started: std::time::Instant,
}

impl ToolSpan {
    pub fn new(name: &str, kind: &str) -> Self {
        let span = tracing::info_span!(
            target: TRACE_TARGET,
            "tool.call",
            otel.name = "tool.call",
            otel.kind = "internal",
            "daimonos.tool.name" = name,
            "daimonos.tool.kind" = kind,
            "daimonos.tool.status" = tracing::field::Empty,
            "daimonos.tool.request_tokens_est" = tracing::field::Empty,
            "daimonos.tool.response_tokens_est" = tracing::field::Empty,
            "daimonos.tool.saved_tokens_est" = tracing::field::Empty,
            "daimonos.tool.redirect" = tracing::field::Empty,
            "daimonos.tool.filtered" = tracing::field::Empty,
            "daimonos.tool.read_dedup" = tracing::field::Empty,
            "daimonos.tool.batch_size" = tracing::field::Empty,
            "daimonos.duration_ms" = tracing::field::Empty,
            "error.type" = tracing::field::Empty,
        );
        Self {
            span,
            started: std::time::Instant::now(),
        }
    }

    pub fn span(&self) -> &tracing::Span {
        &self.span
    }

    /// Record the terminal status plus metadata-only sizes/flags, then close
    /// the span with its measured duration.
    pub fn finish(self, status: ToolStatus, outcome: ToolOutcome) {
        // Enter while recording so the span is active at least once. Native
        // calls are entered via `.instrument()` around the facade await, but
        // blocked/unavailable calls never execute a body; entering here keeps
        // their export path identical to instrumented spans.
        let _entered = self.span.enter();
        self.span.record("daimonos.tool.status", status.as_str());
        self.span.record(
            "daimonos.tool.request_tokens_est",
            outcome.request_tokens_est,
        );
        self.span.record(
            "daimonos.tool.response_tokens_est",
            outcome.response_tokens_est,
        );
        self.span
            .record("daimonos.tool.saved_tokens_est", outcome.saved_tokens_est);
        self.span.record("daimonos.tool.redirect", outcome.redirect);
        self.span.record("daimonos.tool.filtered", outcome.filtered);
        self.span
            .record("daimonos.tool.read_dedup", outcome.read_dedup);
        self.span
            .record("daimonos.tool.batch_size", outcome.batch_size);
        self.span.record(
            "daimonos.duration_ms",
            self.started.elapsed().as_millis() as u64,
        );
        if let Some(error_type) = status.error_type() {
            self.span.record("error.type", error_type);
        }
    }

    /// Close a span for a call that never executed (blocked / unavailable):
    /// records only the status and duration.
    pub fn finish_status(self, status: ToolStatus) {
        self.finish(status, ToolOutcome::default());
    }
}

/// An `mcp.remote_tool` span for a forwarded MCP tool call (ADR-003, D4).
pub struct RemoteToolSpan {
    span: tracing::Span,
    started: std::time::Instant,
}

impl RemoteToolSpan {
    /// `server` is the forwarded-server alias — bounded by config, carrying no
    /// credentials, headers, or URIs (D6).
    pub fn new(name: &str, server: &str) -> Self {
        let span = tracing::info_span!(
            target: TRACE_TARGET,
            "mcp.remote_tool",
            otel.name = "mcp.remote_tool",
            otel.kind = "client",
            "daimonos.tool.name" = name,
            "daimonos.tool.kind" = "remote",
            "daimonos.mcp.server" = server,
            "daimonos.tool.status" = tracing::field::Empty,
            "daimonos.tool.request_tokens_est" = tracing::field::Empty,
            "daimonos.tool.response_tokens_est" = tracing::field::Empty,
            "daimonos.duration_ms" = tracing::field::Empty,
            "error.type" = tracing::field::Empty,
        );
        Self {
            span,
            started: std::time::Instant::now(),
        }
    }

    pub fn span(&self) -> &tracing::Span {
        &self.span
    }

    pub fn finish(self, status: ToolStatus, request_tokens_est: u64, response_tokens_est: u64) {
        self.span.record("daimonos.tool.status", status.as_str());
        self.span
            .record("daimonos.tool.request_tokens_est", request_tokens_est);
        self.span
            .record("daimonos.tool.response_tokens_est", response_tokens_est);
        self.span.record(
            "daimonos.duration_ms",
            self.started.elapsed().as_millis() as u64,
        );
        if let Some(error_type) = status.error_type() {
            self.span.record("error.type", error_type);
        }
    }
}

/// Emit a one-shot MCP bridge lifecycle span. These occur outside any prompt
/// (D4) so they root their own session-lifecycle trace, and carry only an
/// event label, server count, and duration — never URIs, headers, or
/// credentials (D6). `error_class` is a bounded class when the event failed.
pub fn record_bridge_lifecycle(
    event: &str,
    servers: u64,
    duration_ms: u64,
    error_class: Option<&str>,
) {
    let span = tracing::info_span!(
        target: TRACE_TARGET,
        parent: None,
        "mcp.bridge",
        otel.name = "mcp.bridge",
        otel.kind = "internal",
        "daimonos.mcp.event" = event,
        "daimonos.mcp.servers" = servers,
        "daimonos.duration_ms" = duration_ms,
        "error.type" = tracing::field::Empty,
    );
    if let Some(error_class) = error_class {
        span.record("error.type", error_class);
    }
    // Enter once before the span drops: a never-entered span exports
    // unreliably under the OpenTelemetry layer (same reason ToolSpan::finish
    // enters). All attributes are already set at construction.
    let _entered = span.enter();
}

/// Static metadata for a `context.compaction` span, known when compaction
/// begins (ADR-006 D4/D5).
pub struct CompactionMetadata<'a> {
    /// `proactive` (pre-turn high-water) or `reactive_overflow` (post-overflow).
    pub trigger: &'a str,
    /// Normalized [`crate::compaction::CompactionStrategy`] name.
    pub strategy: &'a str,
    pub high_water: f64,
    pub low_water: f64,
    /// Measured/estimated prompt occupancy that led to compaction.
    pub occupancy_tokens: u64,
    /// Summarizer model (bounded by config; low-cardinality).
    pub summary_model: &'a str,
}

/// Numeric result of a compaction, recorded when it completes.
#[derive(Debug, Default, Clone, Copy)]
pub struct CompactionOutcome {
    pub tokens_before_est: u64,
    pub tokens_after_est: u64,
    pub evicted_turns: u64,
    pub evicted_messages: u64,
    /// Summarization retries beyond the first attempt (0 or 1 today).
    pub summary_retries: u64,
    /// Summarizer failed and evicted turns were dropped with a marker.
    pub fallback_drop: bool,
}

/// A `context.compaction` span. Enter it around the summary generation so the
/// `llm.generation` (kind `compaction_summary`) nests beneath it (D4).
pub struct CompactionSpan {
    span: tracing::Span,
    started: std::time::Instant,
}

impl CompactionSpan {
    pub fn new(metadata: CompactionMetadata<'_>) -> Self {
        let span = tracing::info_span!(
            target: TRACE_TARGET,
            "context.compaction",
            otel.name = "context.compaction",
            otel.kind = "internal",
            "daimonos.compaction.trigger" = metadata.trigger,
            "daimonos.compaction.strategy" = metadata.strategy,
            "daimonos.compaction.high_water" = metadata.high_water,
            "daimonos.compaction.low_water" = metadata.low_water,
            "daimonos.context.used" = metadata.occupancy_tokens,
            "daimonos.compaction.summary_model" = metadata.summary_model,
            "daimonos.compaction.tokens_before_est" = tracing::field::Empty,
            "daimonos.compaction.tokens_after_est" = tracing::field::Empty,
            "daimonos.compaction.evicted_turns" = tracing::field::Empty,
            "daimonos.compaction.evicted_messages" = tracing::field::Empty,
            "daimonos.compaction.summary_retries" = tracing::field::Empty,
            "daimonos.compaction.fallback_drop" = tracing::field::Empty,
            "daimonos.duration_ms" = tracing::field::Empty,
        );
        Self {
            span,
            started: std::time::Instant::now(),
        }
    }

    pub fn span(&self) -> &tracing::Span {
        &self.span
    }

    pub fn finish(self, outcome: CompactionOutcome) {
        self.span.record(
            "daimonos.compaction.tokens_before_est",
            outcome.tokens_before_est,
        );
        self.span.record(
            "daimonos.compaction.tokens_after_est",
            outcome.tokens_after_est,
        );
        self.span
            .record("daimonos.compaction.evicted_turns", outcome.evicted_turns);
        self.span.record(
            "daimonos.compaction.evicted_messages",
            outcome.evicted_messages,
        );
        self.span.record(
            "daimonos.compaction.summary_retries",
            outcome.summary_retries,
        );
        self.span
            .record("daimonos.compaction.fallback_drop", outcome.fallback_drop);
        self.span.record(
            "daimonos.duration_ms",
            self.started.elapsed().as_millis() as u64,
        );
    }
}

/// An `agent.retry` span (ADR-006 D4). Enter it around the retried attempt so
/// the retry's `llm.generation` children nest beneath it.
pub struct RetrySpan {
    span: tracing::Span,
    started: std::time::Instant,
}

impl RetrySpan {
    /// `reason` is `context_overflow`, `explicit`, or `transport_recovery`.
    /// `trigger_generation_ordinal` links the retry to the generation that
    /// failed (D5: "context-overflow retry links to failed generation").
    pub fn new(reason: &str, trigger_generation_ordinal: Option<u64>) -> Self {
        let span = tracing::info_span!(
            target: TRACE_TARGET,
            "agent.retry",
            otel.name = "agent.retry",
            otel.kind = "internal",
            "daimonos.retry.reason" = reason,
            "daimonos.retry.trigger_generation.ordinal" = tracing::field::Empty,
            "daimonos.duration_ms" = tracing::field::Empty,
            "error.type" = tracing::field::Empty,
        );
        if let Some(ordinal) = trigger_generation_ordinal {
            span.record("daimonos.retry.trigger_generation.ordinal", ordinal);
        }
        Self {
            span,
            started: std::time::Instant::now(),
        }
    }

    pub fn span(&self) -> &tracing::Span {
        &self.span
    }

    pub fn finish(self, error_type: Option<&str>) {
        self.span.record(
            "daimonos.duration_ms",
            self.started.elapsed().as_millis() as u64,
        );
        if let Some(error_type) = error_type {
            self.span.record("error.type", error_type);
        }
    }
}

/// Emit a one-shot `agent.truncate` span for a user-initiated history
/// truncation (ADR-006 D5). Identifies the removed turn index and counts, no
/// conversation content. Attaches to the active prompt root when one exists,
/// else roots its own trace.
pub fn record_truncation(turn_index: u64, evicted_turns: u64, evicted_messages: u64) {
    let span = tracing::info_span!(
        target: TRACE_TARGET,
        "agent.truncate",
        otel.name = "agent.truncate",
        otel.kind = "internal",
        "daimonos.truncate.turn_index" = turn_index,
        "daimonos.truncate.evicted_turns" = evicted_turns,
        "daimonos.truncate.evicted_messages" = evicted_messages,
    );
    let _entered = span.enter();
}

fn workspace_id(workspace: &std::path::Path) -> String {
    let mut hasher = Sha256::new();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(workspace.as_os_str().as_bytes());
    }
    #[cfg(not(unix))]
    hasher.update(workspace.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    hex::encode(&digest[..16])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservabilityStatus {
    Disabled,
    Active,
    Failed(String),
}

pub struct ObservabilityRuntime {
    provider: Option<SdkTracerProvider>,
    status: ObservabilityStatus,
    flush_timeout: Duration,
}

impl ObservabilityRuntime {
    pub fn initialize(config: &ObservabilityConfig) -> Self {
        let flush_timeout = Duration::from_millis(config.flush_timeout_ms);
        if !config.enabled {
            return Self {
                provider: None,
                status: ObservabilityStatus::Disabled,
                flush_timeout,
            };
        }

        let credentials = match config.resolve_basic_auth() {
            Ok(credentials) => credentials,
            Err(error) => {
                return Self {
                    provider: None,
                    status: ObservabilityStatus::Failed(error),
                    flush_timeout,
                };
            }
        };
        let exporter = match build_exporter(config, credentials.as_ref()) {
            Ok(exporter) => exporter,
            Err(error) => {
                return Self {
                    provider: None,
                    status: ObservabilityStatus::Failed(format!(
                        "OTLP exporter initialization failed: {error}"
                    )),
                    flush_timeout,
                };
            }
        };
        let provider = build_provider(config, LoggingExporter(exporter));

        Self {
            provider: Some(provider),
            status: ObservabilityStatus::Active,
            flush_timeout,
        }
    }

    pub fn status(&self) -> &ObservabilityStatus {
        &self.status
    }

    #[cfg(test)]
    pub fn tracer_provider(&self) -> Option<&SdkTracerProvider> {
        self.provider.as_ref()
    }

    pub fn tracer(&self) -> Option<SdkTracer> {
        self.provider
            .as_ref()
            .map(|provider| provider.tracer("daimonos"))
    }

    pub async fn shutdown(&mut self) {
        if let Some(provider) = self.provider.take() {
            let flush_timeout = self.flush_timeout;
            let shutdown =
                tokio::task::spawn_blocking(move || provider.shutdown_with_timeout(flush_timeout));
            if tokio::time::timeout(flush_timeout, shutdown).await.is_err() {
                tracing::warn!(
                    target: LOCAL_DIAGNOSTIC_TARGET,
                    event = "telemetry_shutdown_failed",
                    timeout_ms = flush_timeout.as_millis() as u64,
                );
            }
        }
    }
}

impl Drop for ObservabilityRuntime {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take() {
            let flush_timeout = self.flush_timeout;
            // Normal runtime paths call async `shutdown` explicitly. This
            // fallback prevents an accidental Drop from blocking a Tokio
            // worker while still giving cleanup a best-effort opportunity.
            let _ = std::thread::Builder::new()
                .name("daimonos-otel-shutdown".to_string())
                .spawn(move || {
                    let _ = provider.shutdown_with_timeout(flush_timeout);
                });
        }
    }
}

#[derive(Debug)]
struct LoggingExporter<E>(E);

impl<E: SpanExporter> SpanExporter for LoggingExporter<E> {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        let result = self.0.export(batch).await;
        if result.is_err() {
            tracing::warn!(
                target: LOCAL_DIAGNOSTIC_TARGET,
                event = "telemetry_export_failed",
            );
        }
        result
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.0.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.0.set_resource(resource);
    }
}

fn build_provider<E: SpanExporter + 'static>(
    config: &ObservabilityConfig,
    exporter: E,
) -> SdkTracerProvider {
    let batch = BatchConfigBuilder::default()
        .with_max_queue_size(config.max_queue_size)
        .with_max_export_batch_size(config.max_batch_size)
        .with_scheduled_delay(Duration::from_millis(config.batch_delay_ms))
        .build();
    let processor = BatchSpanProcessor::builder(exporter)
        .with_batch_config(batch)
        .build();
    let sampler = Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(config.sample_ratio)));
    let mut resource = Resource::builder()
        .with_service_name("daimonos")
        .with_attribute(KeyValue::new(
            "deployment.environment.name",
            config.environment.clone(),
        ))
        .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")));
    if let Some(release) = config.release.as_deref() {
        resource = resource.with_attribute(KeyValue::new("langfuse.release", release.to_string()));
    }
    SdkTracerProvider::builder()
        .with_sampler(sampler)
        .with_resource(resource.build())
        .with_span_processor(processor)
        .build()
}

fn build_exporter(
    config: &ObservabilityConfig,
    credentials: Option<&(String, String)>,
) -> Result<opentelemetry_otlp::SpanExporter, opentelemetry_otlp::ExporterBuildError> {
    let mut builder = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(config.endpoint.clone());
    if let Some((username, password)) = credentials {
        builder = builder.with_headers(HashMap::from([(
            "authorization".to_string(),
            basic_authorization_header(username, password),
        )]));
    }
    builder.build()
}

fn basic_authorization_header(username: &str, password: &str) -> String {
    let credentials = format!("{username}:{password}");
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(credentials)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ObservabilityConfig;
    use opentelemetry::trace::{Span as _, Tracer as _};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::sync::Mutex;

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn disabled_runtime_does_not_resolve_credentials() {
        let config = ObservabilityConfig {
            enabled: false,
            basic_auth_username_env: "DAIMONOS_TEST_MISSING_PUBLIC".to_string(),
            basic_auth_password_env: "DAIMONOS_TEST_MISSING_SECRET".to_string(),
            ..ObservabilityConfig::default()
        };

        let runtime = ObservabilityRuntime::initialize(&config);

        assert_eq!(runtime.status(), &ObservabilityStatus::Disabled);
        assert!(runtime.tracer_provider().is_none());
    }

    #[test]
    fn missing_credentials_fail_open_without_secret_material() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|error| error.into_inner());
        let username_env = "DAIMONOS_TEST_MISSING_PUBLIC";
        let password_env = "DAIMONOS_TEST_MISSING_SECRET";
        std::env::remove_var(username_env);
        std::env::remove_var(password_env);
        let config = ObservabilityConfig {
            enabled: true,
            basic_auth_username_env: username_env.to_string(),
            basic_auth_password_env: password_env.to_string(),
            ..ObservabilityConfig::default()
        };

        let runtime = ObservabilityRuntime::initialize(&config);

        let ObservabilityStatus::Failed(message) = runtime.status() else {
            panic!("missing credentials should disable export");
        };
        assert!(message.contains(username_env));
        assert!(!message.contains("Authorization"));
        assert!(runtime.tracer_provider().is_none());
    }

    #[test]
    fn basic_authorization_header_matches_rfc_7617() {
        assert_eq!(
            basic_authorization_header("public", "secret"),
            "Basic cHVibGljOnNlY3JldA=="
        );
    }

    #[test]
    fn basic_auth_username_rejects_colon() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|error| error.into_inner());
        let username_env = "DAIMONOS_TEST_OTLP_COLON_PUBLIC";
        let password_env = "DAIMONOS_TEST_OTLP_COLON_SECRET";
        std::env::set_var(username_env, "public:invalid");
        std::env::set_var(password_env, "secret");
        let config = ObservabilityConfig {
            enabled: true,
            basic_auth_username_env: username_env.to_string(),
            basic_auth_password_env: password_env.to_string(),
            ..ObservabilityConfig::default()
        };

        let runtime = ObservabilityRuntime::initialize(&config);

        std::env::remove_var(username_env);
        std::env::remove_var(password_env);
        let ObservabilityStatus::Failed(message) = runtime.status() else {
            panic!("colon in Basic Auth username must disable export");
        };
        assert!(message.contains("must not contain ':'"));
        assert!(!message.contains("public:invalid"));
    }

    #[test]
    fn basic_auth_credentials_reject_control_characters() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|error| error.into_inner());
        let username_env = "DAIMONOS_TEST_OTLP_CONTROL_PUBLIC";
        let password_env = "DAIMONOS_TEST_OTLP_CONTROL_SECRET";
        std::env::set_var(username_env, "public");
        std::env::set_var(password_env, "secret\r\ninjected: value");
        let config = ObservabilityConfig {
            enabled: true,
            basic_auth_username_env: username_env.to_string(),
            basic_auth_password_env: password_env.to_string(),
            ..ObservabilityConfig::default()
        };

        let runtime = ObservabilityRuntime::initialize(&config);

        std::env::remove_var(username_env);
        std::env::remove_var(password_env);
        let ObservabilityStatus::Failed(message) = runtime.status() else {
            panic!("control characters in Basic Auth must disable export");
        };
        assert!(message.contains("must not contain control characters"));
        assert!(!message.contains("injected"));
    }

    #[test]
    fn basic_auth_credentials_reject_ambiguous_non_ascii() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|error| error.into_inner());
        let username_env = "DAIMONOS_TEST_OTLP_UNICODE_PUBLIC";
        let password_env = "DAIMONOS_TEST_OTLP_UNICODE_SECRET";
        std::env::set_var(username_env, "public");
        std::env::set_var(password_env, "sëcret");
        let config = ObservabilityConfig {
            enabled: true,
            basic_auth_username_env: username_env.to_string(),
            basic_auth_password_env: password_env.to_string(),
            ..ObservabilityConfig::default()
        };

        let runtime = ObservabilityRuntime::initialize(&config);

        std::env::remove_var(username_env);
        std::env::remove_var(password_env);
        let ObservabilityStatus::Failed(message) = runtime.status() else {
            panic!("non-ASCII Basic Auth must disable export");
        };
        assert!(message.contains("only ASCII"));
        assert!(!message.contains("sëcret"));
    }

    #[derive(Debug)]
    struct SlowExporter {
        exports: Arc<AtomicUsize>,
        delay: Duration,
    }

    impl SpanExporter for SlowExporter {
        async fn export(&self, _batch: Vec<SpanData>) -> OTelSdkResult {
            self.exports.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(self.delay);
            Ok(())
        }
    }

    #[test]
    fn saturated_queue_drops_spans_without_blocking_producer() {
        let config = ObservabilityConfig {
            max_queue_size: 1,
            max_batch_size: 1,
            batch_delay_ms: 1,
            ..ObservabilityConfig::default()
        };
        let exports = Arc::new(AtomicUsize::new(0));
        let provider = build_provider(
            &config,
            SlowExporter {
                exports: Arc::clone(&exports),
                delay: Duration::from_millis(500),
            },
        );
        let tracer = provider.tracer("queue-test");
        let started = std::time::Instant::now();
        for _ in 0..1_000 {
            let mut span = tracer.start("queued");
            span.end();
        }

        assert!(
            started.elapsed() < Duration::from_millis(100),
            "span producers must not wait for exporter backpressure"
        );
        assert!(provider
            .shutdown_with_timeout(Duration::from_millis(20))
            .is_err());
    }

    async fn mock_otlp_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if let Some(header_end) =
                    request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if request.len() >= header_end + 4 + content_length {
                        break;
                    }
                }
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        (address, server)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exports_otlp_http_with_basic_auth() {
        let username_env = "DAIMONOS_TEST_OTLP_PUBLIC";
        let password_env = "DAIMONOS_TEST_OTLP_SECRET";
        let (address, server) = mock_otlp_server().await;
        let mut runtime = {
            let _guard = ENV_MUTEX.lock().unwrap_or_else(|error| error.into_inner());
            std::env::set_var(username_env, "public");
            std::env::set_var(password_env, "secret");
            let config = ObservabilityConfig {
                enabled: true,
                endpoint: format!("http://{address}/api/public/otel/v1/traces"),
                basic_auth_username_env: username_env.to_string(),
                basic_auth_password_env: password_env.to_string(),
                batch_delay_ms: 10,
                flush_timeout_ms: 2_000,
                ..ObservabilityConfig::default()
            };
            let runtime = ObservabilityRuntime::initialize(&config);
            std::env::remove_var(username_env);
            std::env::remove_var(password_env);
            runtime
        };
        assert_eq!(runtime.status(), &ObservabilityStatus::Active);
        let mut span = runtime.tracer().unwrap().start("export-test");
        span.end();
        runtime.shutdown().await;
        let request = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        assert!(request.starts_with("POST /api/public/otel/v1/traces "));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: basic chvibgljonnly3jlda=="));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exports_to_unauthenticated_otlp_collector() {
        let (address, server) = mock_otlp_server().await;
        let config = ObservabilityConfig {
            enabled: true,
            endpoint: format!("http://{address}/v1/traces"),
            basic_auth: false,
            batch_delay_ms: 10,
            flush_timeout_ms: 2_000,
            ..ObservabilityConfig::default()
        };
        let mut runtime = ObservabilityRuntime::initialize(&config);
        assert_eq!(runtime.status(), &ObservabilityStatus::Active);
        let mut span = runtime.tracer().unwrap().start("unauthenticated");
        span.end();
        runtime.shutdown().await;
        let request = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();

        assert!(request.starts_with("POST /v1/traces "));
        assert!(!request.to_ascii_lowercase().contains("authorization:"));
    }

    #[test]
    fn prompt_roots_export_safe_metadata_and_distinct_traces() {
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
        use tracing_subscriber::layer::SubscriberExt;

        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("prompt-root-test")));
        tracing::subscriber::with_default(subscriber, || {
            for turn in 0..2 {
                let prompt = PromptSpan::new(PromptMetadata {
                    mode: "acp",
                    session_id: Some("session-123"),
                    model: "test-model",
                    workspace: std::path::Path::new("/private/workspace"),
                    turn_index: turn,
                    tools_exposed: 17,
                });
                prompt.span().in_scope(|| {
                    if turn == 0 {
                        tracing::Span::current().record("error.type", "provider_error");
                    }
                });
                prompt.finish("end_turn", None);
            }
        });
        provider.force_flush().unwrap();

        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 2);
        assert_ne!(
            spans[0].span_context.trace_id(),
            spans[1].span_context.trace_id()
        );
        assert!(spans[0].attributes.iter().any(|attribute| {
            attribute.key.as_str() == "error.type"
                && attribute.value.to_string() == "provider_error"
        }));
        for span in spans {
            let attributes = span
                .attributes
                .iter()
                .map(|attribute| (attribute.key.as_str(), attribute.value.to_string()))
                .collect::<std::collections::HashMap<_, _>>();
            assert_eq!(
                attributes.get("langfuse.session.id").map(String::as_str),
                Some("session-123")
            );
            assert_eq!(
                attributes.get("daimonos.runtime.mode").map(String::as_str),
                Some("acp")
            );
            assert_eq!(
                attributes.get("gen_ai.request.model").map(String::as_str),
                Some("test-model")
            );
            assert!(!format!("{attributes:?}").contains("/private/workspace"));
        }
    }

    #[test]
    fn generation_span_records_time_to_first_token() {
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
        use tracing_subscriber::layer::SubscriberExt;

        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("ttft-test")));
        // Fully synchronous span lifecycle under `with_default` — the
        // deterministic pattern (no tokio/async in the export path).
        tracing::subscriber::with_default(subscriber, || {
            let generation = GenerationSpan::new(GenerationMetadata {
                kind: "agent",
                model: "test-model",
                max_tokens: 100,
                thinking: crate::providers::ThinkingLevel::Off,
                temperature: None,
                ordinal: 0,
                tools_exposed: 0,
                stable_prefix_len: 0,
            });
            // The first observed token records time-to-first-token (ADR-006).
            generation.mark_first_token();
            generation.finish(&crate::providers::LlmResponse {
                content: vec![],
                stop_reason: crate::providers::StopReason::EndTurn,
                error_message: None,
                context_overflow: false,
                usage: crate::providers::Usage::default(),
            });
        });
        provider.force_flush().unwrap();

        let spans = exporter.get_finished_spans().unwrap();
        let generation = spans
            .iter()
            .find(|span| span.name == "llm.generation")
            .expect("llm.generation span must be exported");
        assert!(generation
            .attributes
            .iter()
            .any(|attribute| attribute.key.as_str() == "daimonos.time_to_first_token_ms"));
    }

    #[test]
    fn prompt_root_omits_absent_session_id() {
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
        use tracing_subscriber::layer::SubscriberExt;

        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("no-session-test")));
        tracing::subscriber::with_default(subscriber, || {
            PromptSpan::new(PromptMetadata {
                mode: "agent",
                session_id: None,
                model: "test-model",
                workspace: std::path::Path::new("/workspace"),
                turn_index: 0,
                tools_exposed: 1,
            })
            .finish("end_turn", None);
        });
        provider.force_flush().unwrap();

        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 1);
        assert!(!spans[0]
            .attributes
            .iter()
            .any(|attribute| attribute.key.as_str() == "langfuse.session.id"));
    }

    #[cfg(unix)]
    #[test]
    fn workspace_hash_preserves_non_utf8_bytes() {
        use std::os::unix::ffi::OsStringExt;

        let first = std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0x80]));
        let second = std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0x81]));

        assert_ne!(workspace_id(&first), workspace_id(&second));
    }

    #[test]
    fn generation_span_is_child_and_exports_usage_without_content() {
        use crate::providers::{Cost, LlmResponse, StopReason, ThinkingLevel, Usage};
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
        use tracing_subscriber::layer::SubscriberExt;

        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("generation-test")));
        tracing::subscriber::with_default(subscriber, || {
            let prompt = PromptSpan::new(PromptMetadata {
                mode: "agent",
                session_id: Some("session"),
                model: "test-model",
                workspace: std::path::Path::new("/workspace"),
                turn_index: 0,
                tools_exposed: 3,
            });
            prompt.span().in_scope(|| {
                let generation = GenerationSpan::new(GenerationMetadata {
                    kind: "agent",
                    model: "test-model",
                    max_tokens: 4_096,
                    thinking: ThinkingLevel::High,
                    temperature: Some(0.2),
                    ordinal: 0,
                    tools_exposed: 3,
                    stable_prefix_len: 2,
                });
                generation.mark_first_token();
                generation.finish(&LlmResponse {
                    content: Vec::new(),
                    stop_reason: StopReason::EndTurn,
                    error_message: None,
                    context_overflow: false,
                    usage: Usage {
                        input: 100,
                        output: 20,
                        cache_read: 30,
                        cache_write: 10,
                        cost: Cost {
                            total_usd: 0.0123,
                            ..Cost::default()
                        },
                    },
                });
            });
            prompt.finish("end_turn", None);
        });
        provider.force_flush().unwrap();

        let spans = exporter.get_finished_spans().unwrap();
        let prompt = spans
            .iter()
            .find(|span| span.name == "agent.prompt")
            .unwrap();
        let generation = spans
            .iter()
            .find(|span| span.name == "llm.generation")
            .unwrap();
        assert_eq!(
            generation.span_context.trace_id(),
            prompt.span_context.trace_id()
        );
        assert_eq!(generation.parent_span_id, prompt.span_context.span_id());
        let attributes = generation
            .attributes
            .iter()
            .map(|attribute| (attribute.key.as_str(), attribute.value.to_string()))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            attributes
                .get("gen_ai.usage.input_tokens")
                .map(String::as_str),
            Some("100")
        );
        assert_eq!(
            attributes
                .get("daimonos.usage.cache_read")
                .map(String::as_str),
            Some("30")
        );
        assert!(attributes["langfuse.observation.model.parameters"].contains("\"max_tokens\":4096"));
        assert!(!attributes.contains_key("langfuse.observation.input"));
        assert!(!attributes.contains_key("langfuse.observation.output"));
    }

    #[test]
    fn tool_kind_classifies_native_plugin_and_script() {
        assert_eq!(tool_kind("read_file"), "native");
        assert_eq!(tool_kind("exec"), "native");
        assert_eq!(tool_kind("git"), "plugin");
        assert_eq!(tool_kind("cargo"), "plugin");
        assert_eq!(tool_kind("execute_script"), "script");
        assert_eq!(tool_kind("mcp__srv__tool"), "native");
    }

    #[test]
    fn remote_server_alias_parses_namespaced_names() {
        assert_eq!(remote_server_alias("mcp__vikunja__tasks"), Some("vikunja"));
        // Collision suffix attaches to the tool segment, not the server.
        assert_eq!(
            remote_server_alias("mcp__vikunja__tasks__2"),
            Some("vikunja")
        );
        assert_eq!(remote_server_alias("read_file"), None);
        assert_eq!(remote_server_alias("mcp__only"), None);
    }

    /// Build an in-memory-exporter subscriber for the span tests below.
    fn in_memory_subscriber(
        name: &'static str,
    ) -> (
        opentelemetry_sdk::trace::InMemorySpanExporter,
        opentelemetry_sdk::trace::SdkTracerProvider,
        impl tracing::Subscriber,
    ) {
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
        use tracing_subscriber::layer::SubscriberExt;

        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer(name)));
        (exporter, provider, subscriber)
    }

    fn attribute_map(
        span: &opentelemetry_sdk::trace::SpanData,
    ) -> std::collections::HashMap<String, String> {
        span.attributes
            .iter()
            .map(|attribute| (attribute.key.to_string(), attribute.value.to_string()))
            .collect()
    }

    #[test]
    fn tool_call_span_is_child_and_metadata_only() {
        let (exporter, provider, subscriber) = in_memory_subscriber("tool-call-test");
        tracing::subscriber::with_default(subscriber, || {
            let prompt = PromptSpan::new(PromptMetadata {
                mode: "agent",
                session_id: Some("session"),
                model: "test-model",
                workspace: std::path::Path::new("/private/workspace"),
                turn_index: 0,
                tools_exposed: 3,
            });
            prompt.span().in_scope(|| {
                let tool = ToolSpan::new("exec", tool_kind("exec"));
                tool.span().in_scope(|| {
                    // Simulate work done under the span; no content recorded.
                });
                tool.finish(
                    ToolStatus::Success,
                    ToolOutcome {
                        request_tokens_est: 12,
                        response_tokens_est: 4,
                        saved_tokens_est: 30,
                        redirect: true,
                        filtered: true,
                        read_dedup: false,
                        batch_size: 1,
                    },
                );
            });
            prompt.finish("end_turn", None);
        });
        provider.force_flush().unwrap();

        let spans = exporter.get_finished_spans().unwrap();
        let prompt = spans.iter().find(|s| s.name == "agent.prompt").unwrap();
        let tool = spans.iter().find(|s| s.name == "tool.call").unwrap();
        assert_eq!(tool.span_context.trace_id(), prompt.span_context.trace_id());
        assert_eq!(tool.parent_span_id, prompt.span_context.span_id());
        let attributes = attribute_map(tool);
        assert_eq!(
            attributes.get("daimonos.tool.name").map(String::as_str),
            Some("exec")
        );
        assert_eq!(
            attributes.get("daimonos.tool.kind").map(String::as_str),
            Some("native")
        );
        assert_eq!(
            attributes.get("daimonos.tool.status").map(String::as_str),
            Some("success")
        );
        assert_eq!(
            attributes.get("daimonos.tool.redirect").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            attributes
                .get("daimonos.tool.saved_tokens_est")
                .map(String::as_str),
            Some("30")
        );
        assert!(!attributes.contains_key("error.type"));
        // Metadata-only: no argument/result/command bodies leak (D6).
        let rendered = format!("{attributes:?}");
        assert!(!rendered.contains("/private/workspace"));
    }

    #[test]
    fn blocked_tool_span_records_status_without_sizes() {
        let (exporter, provider, subscriber) = in_memory_subscriber("blocked-tool-test");
        tracing::subscriber::with_default(subscriber, || {
            let tool = ToolSpan::new("write_file", "native");
            tool.finish_status(ToolStatus::Blocked);
        });
        provider.force_flush().unwrap();

        let spans = exporter.get_finished_spans().unwrap();
        let tool = spans.iter().find(|s| s.name == "tool.call").unwrap();
        let attributes = attribute_map(tool);
        assert_eq!(
            attributes.get("daimonos.tool.status").map(String::as_str),
            Some("blocked")
        );
        assert_eq!(
            attributes.get("error.type").map(String::as_str),
            Some("blocked")
        );
    }

    #[test]
    fn tool_status_strings_and_error_classes_are_bounded() {
        assert_eq!(ToolStatus::Success.as_str(), "success");
        assert_eq!(ToolStatus::Error.as_str(), "error");
        assert_eq!(ToolStatus::Blocked.as_str(), "blocked");
        assert_eq!(ToolStatus::Timeout.as_str(), "timeout");
        assert_eq!(ToolStatus::Unavailable.as_str(), "unavailable");
        assert_eq!(ToolStatus::Success.error_type(), None);
        assert_eq!(ToolStatus::Error.error_type(), Some("tool_error"));
        assert_eq!(ToolStatus::Blocked.error_type(), Some("blocked"));
        assert_eq!(ToolStatus::Timeout.error_type(), Some("timeout"));
        assert_eq!(ToolStatus::Unavailable.error_type(), Some("unavailable"));
    }

    #[test]
    fn remote_tool_span_records_server_and_timeout() {
        let (exporter, provider, subscriber) = in_memory_subscriber("remote-tool-test");
        tracing::subscriber::with_default(subscriber, || {
            let prompt = PromptSpan::new(PromptMetadata {
                mode: "acp",
                session_id: Some("session"),
                model: "test-model",
                workspace: std::path::Path::new("/workspace"),
                turn_index: 0,
                tools_exposed: 1,
            });
            prompt.span().in_scope(|| {
                let remote = RemoteToolSpan::new("mcp__vikunja__tasks", "vikunja");
                remote.finish(ToolStatus::Timeout, 10, 0);
            });
            prompt.finish("end_turn", None);
        });
        provider.force_flush().unwrap();

        let spans = exporter.get_finished_spans().unwrap();
        let prompt = spans.iter().find(|s| s.name == "agent.prompt").unwrap();
        let remote = spans.iter().find(|s| s.name == "mcp.remote_tool").unwrap();
        assert_eq!(
            remote.span_context.trace_id(),
            prompt.span_context.trace_id()
        );
        assert_eq!(remote.parent_span_id, prompt.span_context.span_id());
        let attributes = attribute_map(remote);
        assert_eq!(
            attributes.get("daimonos.mcp.server").map(String::as_str),
            Some("vikunja")
        );
        assert_eq!(
            attributes.get("daimonos.tool.kind").map(String::as_str),
            Some("remote")
        );
        assert_eq!(
            attributes.get("daimonos.tool.status").map(String::as_str),
            Some("timeout")
        );
        assert_eq!(
            attributes.get("error.type").map(String::as_str),
            Some("timeout")
        );
    }

    #[test]
    fn bridge_lifecycle_span_roots_own_trace_without_credentials() {
        let (exporter, provider, subscriber) = in_memory_subscriber("bridge-lifecycle-test");
        tracing::subscriber::with_default(subscriber, || {
            record_bridge_lifecycle("build", 2, 42, None);
        });
        provider.force_flush().unwrap();

        let spans = exporter.get_finished_spans().unwrap();
        let bridge = spans.iter().find(|s| s.name == "mcp.bridge").unwrap();
        // No prompt parent: lifecycle spans root their own trace (D4).
        assert!(!bridge.parent_span_id.to_string().chars().any(|c| c != '0'));
        let attributes = attribute_map(bridge);
        assert_eq!(
            attributes.get("daimonos.mcp.event").map(String::as_str),
            Some("build")
        );
        assert_eq!(
            attributes.get("daimonos.mcp.servers").map(String::as_str),
            Some("2")
        );
        assert!(!attributes.contains_key("error.type"));
    }

    #[test]
    fn compaction_span_records_metadata_and_nests_summary_generation() {
        use crate::providers::{Cost, LlmResponse, StopReason, ThinkingLevel, Usage};

        let (exporter, provider, subscriber) = in_memory_subscriber("compaction-test");
        tracing::subscriber::with_default(subscriber, || {
            let prompt = PromptSpan::new(PromptMetadata {
                mode: "chat",
                session_id: Some("s"),
                model: "m",
                workspace: std::path::Path::new("/w"),
                turn_index: 0,
                tools_exposed: 0,
            });
            prompt.span().in_scope(|| {
                let compaction = CompactionSpan::new(CompactionMetadata {
                    trigger: "reactive_overflow",
                    strategy: "summarize",
                    high_water: 0.75,
                    low_water: 0.5,
                    occupancy_tokens: 900,
                    summary_model: "summary-model",
                });
                compaction.span().in_scope(|| {
                    let generation = GenerationSpan::new(GenerationMetadata {
                        kind: "compaction_summary",
                        model: "summary-model",
                        max_tokens: 256,
                        thinking: ThinkingLevel::Off,
                        temperature: Some(0.0),
                        ordinal: 3,
                        tools_exposed: 0,
                        stable_prefix_len: 0,
                    });
                    generation.finish(&LlmResponse {
                        content: Vec::new(),
                        stop_reason: StopReason::EndTurn,
                        error_message: None,
                        context_overflow: false,
                        usage: Usage {
                            input: 10,
                            output: 5,
                            cache_read: 0,
                            cache_write: 0,
                            cost: Cost::default(),
                        },
                    });
                });
                compaction.finish(CompactionOutcome {
                    tokens_before_est: 900,
                    tokens_after_est: 400,
                    evicted_turns: 4,
                    evicted_messages: 12,
                    summary_retries: 1,
                    fallback_drop: false,
                });
            });
            prompt.finish("end_turn", None);
        });
        provider.force_flush().unwrap();

        let spans = exporter.get_finished_spans().unwrap();
        let prompt = spans.iter().find(|s| s.name == "agent.prompt").unwrap();
        let compaction = spans
            .iter()
            .find(|s| s.name == "context.compaction")
            .unwrap();
        let generation = spans.iter().find(|s| s.name == "llm.generation").unwrap();
        assert_eq!(compaction.parent_span_id, prompt.span_context.span_id());
        assert_eq!(
            generation.parent_span_id,
            compaction.span_context.span_id(),
            "summary generation must nest under context.compaction"
        );
        let attributes = attribute_map(compaction);
        assert_eq!(
            attributes
                .get("daimonos.compaction.trigger")
                .map(String::as_str),
            Some("reactive_overflow")
        );
        assert_eq!(
            attributes
                .get("daimonos.compaction.tokens_after_est")
                .map(String::as_str),
            Some("400")
        );
        assert_eq!(
            attributes
                .get("daimonos.compaction.evicted_turns")
                .map(String::as_str),
            Some("4")
        );
        assert_eq!(
            attributes
                .get("daimonos.compaction.summary_retries")
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(
            attributes
                .get("daimonos.compaction.fallback_drop")
                .map(String::as_str),
            Some("false")
        );
        assert_eq!(
            attributes
                .get("daimonos.compaction.summary_model")
                .map(String::as_str),
            Some("summary-model")
        );
    }

    #[test]
    fn retry_span_records_reason_and_trigger_ordinal() {
        let (exporter, provider, subscriber) = in_memory_subscriber("retry-test");
        tracing::subscriber::with_default(subscriber, || {
            let prompt = PromptSpan::new(PromptMetadata {
                mode: "acp",
                session_id: Some("s"),
                model: "m",
                workspace: std::path::Path::new("/w"),
                turn_index: 0,
                tools_exposed: 0,
            });
            prompt.span().in_scope(|| {
                let retry = RetrySpan::new("context_overflow", Some(2));
                retry.finish(None);
            });
            prompt.finish("end_turn", None);
        });
        provider.force_flush().unwrap();

        let spans = exporter.get_finished_spans().unwrap();
        let prompt = spans.iter().find(|s| s.name == "agent.prompt").unwrap();
        let retry = spans.iter().find(|s| s.name == "agent.retry").unwrap();
        assert_eq!(retry.parent_span_id, prompt.span_context.span_id());
        let attributes = attribute_map(retry);
        assert_eq!(
            attributes.get("daimonos.retry.reason").map(String::as_str),
            Some("context_overflow")
        );
        assert_eq!(
            attributes
                .get("daimonos.retry.trigger_generation.ordinal")
                .map(String::as_str),
            Some("2")
        );
    }

    #[test]
    fn truncation_span_records_turn_index_and_counts() {
        let (exporter, provider, subscriber) = in_memory_subscriber("truncate-test");
        tracing::subscriber::with_default(subscriber, || {
            record_truncation(2, 3, 9);
        });
        provider.force_flush().unwrap();

        let spans = exporter.get_finished_spans().unwrap();
        let truncate = spans.iter().find(|s| s.name == "agent.truncate").unwrap();
        let attributes = attribute_map(truncate);
        assert_eq!(
            attributes
                .get("daimonos.truncate.turn_index")
                .map(String::as_str),
            Some("2")
        );
        assert_eq!(
            attributes
                .get("daimonos.truncate.evicted_messages")
                .map(String::as_str),
            Some("9")
        );
    }

    #[test]
    fn prompt_records_cancel_reason() {
        let (exporter, provider, subscriber) = in_memory_subscriber("cancel-test");
        tracing::subscriber::with_default(subscriber, || {
            let prompt = PromptSpan::new(PromptMetadata {
                mode: "acp",
                session_id: Some("s"),
                model: "m",
                workspace: std::path::Path::new("/w"),
                turn_index: 0,
                tools_exposed: 0,
            });
            prompt.record_cancel_reason("client");
            prompt.finish("cancelled", Some("client_cancelled"));
        });
        provider.force_flush().unwrap();

        let spans = exporter.get_finished_spans().unwrap();
        let prompt = spans.iter().find(|s| s.name == "agent.prompt").unwrap();
        let attributes = attribute_map(prompt);
        assert_eq!(
            attributes.get("daimonos.cancel.reason").map(String::as_str),
            Some("client")
        );
    }

    // --- #1041 validation: hierarchy, privacy, overhead ---

    use crate::providers::{Cost, LlmResponse, StopReason, ThinkingLevel, Usage};

    fn sample_generation(ordinal: u64, kind: &'static str) -> GenerationSpan {
        GenerationSpan::new(GenerationMetadata {
            kind,
            model: "test-model",
            max_tokens: 1024,
            thinking: ThinkingLevel::Off,
            temperature: Some(0.0),
            ordinal,
            tools_exposed: 1,
            stable_prefix_len: 0,
        })
    }

    fn sample_response() -> LlmResponse {
        LlmResponse {
            content: Vec::new(),
            stop_reason: StopReason::EndTurn,
            error_message: None,
            context_overflow: false,
            usage: Usage {
                input: 100,
                output: 20,
                cache_read: 0,
                cache_write: 0,
                cost: Cost {
                    total_usd: 0.01,
                    ..Cost::default()
                },
            },
        }
    }

    /// End-to-end validation of the ADR-006 D4 hierarchy in one prompt trace:
    /// generation, tool.call, mcp.remote_tool, context.compaction (with a
    /// nested summary generation), and agent.retry (with a nested generation)
    /// all share the prompt's trace and nest under the correct parent. This
    /// is the deterministic local stand-in for the Langfuse smoke test.
    #[test]
    fn full_prompt_trace_hierarchy_nests_all_observation_types() {
        let (exporter, provider, subscriber) = in_memory_subscriber("full-hierarchy");
        tracing::subscriber::with_default(subscriber, || {
            let prompt = PromptSpan::new(PromptMetadata {
                mode: "acp",
                session_id: Some("sess"),
                model: "test-model",
                workspace: std::path::Path::new("/w"),
                turn_index: 0,
                tools_exposed: 2,
            });
            prompt.span().in_scope(|| {
                let generation = sample_generation(0, "agent");
                generation.mark_first_token();
                generation.finish(&sample_response());

                let tool = ToolSpan::new("read_file", "native");
                tool.finish(
                    ToolStatus::Success,
                    ToolOutcome {
                        batch_size: 1,
                        ..ToolOutcome::default()
                    },
                );

                let remote = RemoteToolSpan::new("mcp__srv__echo", "srv");
                remote.finish(ToolStatus::Success, 1, 1);

                let compaction = CompactionSpan::new(CompactionMetadata {
                    trigger: "proactive",
                    strategy: "summarize",
                    high_water: 0.75,
                    low_water: 0.5,
                    occupancy_tokens: 800,
                    summary_model: "test-model",
                });
                compaction.span().in_scope(|| {
                    let summary = sample_generation(1, "compaction_summary");
                    summary.finish(&sample_response());
                });
                compaction.finish(CompactionOutcome {
                    tokens_before_est: 800,
                    tokens_after_est: 400,
                    evicted_turns: 2,
                    evicted_messages: 6,
                    summary_retries: 0,
                    fallback_drop: false,
                });

                let retry = RetrySpan::new("context_overflow", Some(0));
                retry.span().in_scope(|| {
                    let retried = sample_generation(2, "agent");
                    retried.finish(&sample_response());
                });
                retry.finish(None);
            });
            prompt.finish("end_turn", None);
        });
        provider.force_flush().unwrap();

        let spans = exporter.get_finished_spans().unwrap();
        let by_name = |name: &str| spans.iter().filter(|s| s.name == name).collect::<Vec<_>>();
        let prompt = by_name("agent.prompt");
        assert_eq!(prompt.len(), 1);
        let prompt = prompt[0];
        let trace = prompt.span_context.trace_id();

        assert_eq!(by_name("tool.call").len(), 1);
        assert_eq!(by_name("mcp.remote_tool").len(), 1);
        assert_eq!(by_name("context.compaction").len(), 1);
        assert_eq!(by_name("agent.retry").len(), 1);
        assert_eq!(by_name("llm.generation").len(), 3);

        // Every span belongs to the one prompt trace.
        for span in &spans {
            assert_eq!(
                span.span_context.trace_id(),
                trace,
                "span {} escaped the prompt trace",
                span.name
            );
        }

        let prompt_id = prompt.span_context.span_id();
        for name in [
            "tool.call",
            "mcp.remote_tool",
            "context.compaction",
            "agent.retry",
        ] {
            assert_eq!(
                by_name(name)[0].parent_span_id,
                prompt_id,
                "{name} must be a direct child of agent.prompt"
            );
        }

        // The three generations nest under prompt, compaction, and retry.
        let compaction_id = by_name("context.compaction")[0].span_context.span_id();
        let retry_id = by_name("agent.retry")[0].span_context.span_id();
        let generation_parents: std::collections::HashSet<_> = by_name("llm.generation")
            .iter()
            .map(|s| s.parent_span_id)
            .collect();
        assert!(
            generation_parents.contains(&prompt_id),
            "agent generation under prompt"
        );
        assert!(
            generation_parents.contains(&compaction_id),
            "summary generation under context.compaction"
        );
        assert!(
            generation_parents.contains(&retry_id),
            "retry generation under agent.retry"
        );
    }

    /// Privacy gate (ADR-006 D6): drive the full hierarchy with a secret-
    /// bearing workspace path and assert no exported span attribute leaks the
    /// raw path (only its one-way hash), and that no forbidden content key
    /// (prompt/output/thinking bodies) is present on any span.
    #[test]
    fn exported_spans_contain_no_sensitive_values() {
        const SECRET_WORKSPACE: &str = "/home/dev/acme-topsecret-XYZZYPLUGH";

        let (exporter, provider, subscriber) = in_memory_subscriber("secret-corpus");
        tracing::subscriber::with_default(subscriber, || {
            let prompt = PromptSpan::new(PromptMetadata {
                mode: "acp",
                session_id: Some("sess"),
                model: "test-model",
                workspace: std::path::Path::new(SECRET_WORKSPACE),
                turn_index: 0,
                tools_exposed: 1,
            });
            prompt.span().in_scope(|| {
                sample_generation(0, "agent").finish(&sample_response());
                ToolSpan::new("read_file", "native")
                    .finish(ToolStatus::Success, ToolOutcome::default());
                RemoteToolSpan::new("mcp__srv__echo", "srv").finish(ToolStatus::Success, 1, 1);
            });
            prompt.finish("end_turn", None);
        });
        provider.force_flush().unwrap();

        let spans = exporter.get_finished_spans().unwrap();
        let expected_hash = workspace_id(std::path::Path::new(SECRET_WORKSPACE));
        let mut saw_hash = false;
        for span in &spans {
            for attribute in span.attributes.iter() {
                let key = attribute.key.as_str();
                let value = attribute.value.to_string();
                assert!(
                    !value.contains(SECRET_WORKSPACE) && !value.contains("XYZZYPLUGH"),
                    "span {} attribute {key} leaked the raw workspace path",
                    span.name
                );
                // Forbidden content keys must never appear (D6).
                for forbidden in [
                    "langfuse.observation.input",
                    "langfuse.observation.output",
                    "daimonos.prompt.text",
                    "daimonos.thinking.text",
                ] {
                    assert_ne!(key, forbidden, "forbidden content key {forbidden} exported");
                }
                if key == "daimonos.workspace.id" && value == expected_hash {
                    saw_hash = true;
                }
            }
        }
        assert!(
            saw_hash,
            "workspace correlation must be present as a one-way hash"
        );
    }

    /// Overhead guard: creating + finishing many spans under an active tracer
    /// must stay well within a generous ceiling. Real cost is ~microseconds
    /// per span; the loose bound only catches pathological regressions without
    /// flaking on shared CI runners. Precise budgets live in docs/observability.md.
    #[test]
    fn enabled_tracing_overhead_is_bounded() {
        let iterations = 5_000;
        let (_exporter, provider, subscriber) = in_memory_subscriber("overhead");
        let started = std::time::Instant::now();
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..iterations {
                let tool = ToolSpan::new("read_file", "native");
                tool.finish(
                    ToolStatus::Success,
                    ToolOutcome {
                        batch_size: 1,
                        ..ToolOutcome::default()
                    },
                );
            }
        });
        // Measure only the turn-thread span create+finish cost; draining the
        // exporter is background work in production (BatchSpanProcessor) and
        // is excluded from the budget.
        let elapsed = started.elapsed();
        provider.force_flush().unwrap();
        assert!(
            elapsed < Duration::from_secs(5),
            "enabled span overhead {elapsed:?} for {iterations} spans exceeds the CI ceiling"
        );
    }

    /// Optional self-hosted Langfuse (or any OTLP) smoke test (ADR-006 D10.7).
    /// Ignored by default — it needs a reachable endpoint and real
    /// credentials, so it never runs in CI. Run manually after starting a
    /// local Langfuse (see docs/observability.md):
    ///
    /// ```text
    /// export LANGFUSE_PUBLIC_KEY=pk-... LANGFUSE_SECRET_KEY=sk-...
    /// export DAIMONOS_SMOKE_OTLP_ENDPOINT=http://localhost:3000/api/public/otel/v1/traces
    /// cargo test --bin daimonos observability::tests::self_hosted_langfuse_smoke_test -- --ignored --nocapture
    /// ```
    ///
    /// It emits one prompt trace covering a generation, a tool call, and a
    /// compaction (with its summary generation) so an operator can verify the
    /// hierarchy, session grouping, usage/cost, TTFT, and tool/compaction
    /// attributes render in the Langfuse UI.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires a live Langfuse/OTLP endpoint + credentials; run manually with --ignored"]
    async fn self_hosted_langfuse_smoke_test() {
        use tracing_subscriber::layer::SubscriberExt;

        let endpoint = std::env::var("DAIMONOS_SMOKE_OTLP_ENDPOINT")
            .unwrap_or_else(|_| "http://localhost:3000/api/public/otel/v1/traces".to_string());
        let config = ObservabilityConfig {
            enabled: true,
            endpoint,
            environment: "smoke-test".to_string(),
            batch_delay_ms: 200,
            flush_timeout_ms: 5_000,
            ..ObservabilityConfig::default()
        };
        let mut runtime = ObservabilityRuntime::initialize(&config);
        assert_eq!(
            runtime.status(),
            &ObservabilityStatus::Active,
            "smoke test needs valid local config + LANGFUSE_PUBLIC_KEY/LANGFUSE_SECRET_KEY set; \
             initialize validates config/credentials only (not endpoint reachability) — confirm \
             delivery in the Langfuse UI"
        );
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(runtime.tracer().unwrap()));
        tracing::subscriber::with_default(subscriber, || {
            let prompt = PromptSpan::new(PromptMetadata {
                mode: "agent",
                session_id: Some("smoke-session"),
                model: "smoke-model",
                workspace: std::path::Path::new("/smoke/workspace"),
                turn_index: 0,
                tools_exposed: 1,
            });
            prompt.span().in_scope(|| {
                let generation = sample_generation(0, "agent");
                generation.mark_first_token();
                generation.finish(&sample_response());
                ToolSpan::new("read_file", "native").finish(
                    ToolStatus::Success,
                    ToolOutcome {
                        batch_size: 1,
                        ..ToolOutcome::default()
                    },
                );
                let compaction = CompactionSpan::new(CompactionMetadata {
                    trigger: "proactive",
                    strategy: "summarize",
                    high_water: 0.75,
                    low_water: 0.5,
                    occupancy_tokens: 800,
                    summary_model: "smoke-model",
                });
                compaction.span().in_scope(|| {
                    sample_generation(1, "compaction_summary").finish(&sample_response());
                });
                compaction.finish(CompactionOutcome {
                    tokens_before_est: 800,
                    tokens_after_est: 400,
                    evicted_turns: 1,
                    evicted_messages: 3,
                    summary_retries: 0,
                    fallback_drop: false,
                });
            });
            prompt.finish("end_turn", None);
        });
        runtime.shutdown().await;
    }
}
