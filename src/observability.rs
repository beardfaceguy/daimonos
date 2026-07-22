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
    name.strip_prefix("mcp__")
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
}
