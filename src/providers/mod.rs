#![allow(dead_code)]

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod anthropic;
pub mod openai;
pub mod openrouter;
pub(crate) mod request_trace;
pub mod router;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentBlock {
    Text(String),
    Image {
        data: String,
        media_type: String,
        uri: Option<String>,
    },
    Thinking(String),
    /// Provider-owned opaque continuation state required to preserve a
    /// stateless tool loop (for example OpenAI encrypted reasoning items).
    /// Persisted in history, never rendered, and replayed only by the matching
    /// provider adapter.
    ProviderState {
        provider: String,
        data: Value,
    },
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text(text.into())],
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text(text.into())],
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Token usage for one API call, in **canonical semantics** (ADR-002,
/// sharpening ADR-001's neutral shape): `input` is the *non-cached* prompt
/// tokens; `cache_read`/`cache_write` are the cached portions. Every
/// provider maps its own wire fields so these meanings hold — Anthropic's
/// `input_tokens` already excludes cache; OpenAI-format `prompt_tokens`
/// *includes* it, so that parser subtracts the `prompt_tokens_details`
/// sub-counts out of `input`.
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    /// Reasoning-token detail already included in `output`; informational only
    /// and never added again when computing totals/cost. `None` means the
    /// provider does not report this detail; `Some(0)` is a reported zero.
    pub reasoning_output: Option<u64>,
    /// Exact UTF-8 bytes emitted through `ThinkingDelta` for this call. This is
    /// deliberately byte-denominated; it is never presented as a token count.
    pub thinking_bytes: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: Cost,
}

impl Usage {
    /// Total tokens the prompt occupied in the model's context window — the
    /// number the compaction trigger (ADR-002) compares against the budget.
    /// Cached tokens still occupy the window, so they count. Thanks to the
    /// canonical field semantics above, this is correct for every provider.
    pub fn prompt_tokens(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }
}

#[derive(Debug, Clone, Default)]
pub struct Cost {
    pub input_usd: f64,
    pub output_usd: f64,
    pub cache_read_usd: f64,
    pub cache_write_usd: f64,
    pub total_usd: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    Error,
    Refusal,
    Aborted,
    MaxTokens,
}

impl StopReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            StopReason::EndTurn => "end_turn",
            StopReason::ToolUse => "tool_use",
            StopReason::Error => "error",
            StopReason::Refusal => "refusal",
            StopReason::Aborted => "aborted",
            StopReason::MaxTokens => "max_tokens",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    #[default]
    Medium,
    High,
    XHigh,
    Max,
}

impl ThinkingLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Every level as its canonical string, in ascending-effort order. This is
    /// the single source of the string<->enum mapping (paired with [`as_str`]),
    /// so parsers and error messages stay consistent as levels are added.
    pub const ALL: [Self; 7] = [
        Self::Off,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];

    /// Parse a reasoning-effort level from a user-supplied string
    /// (case-insensitive, surrounding whitespace ignored). Reuses [`as_str`]
    /// as the canonical mapping so there is exactly one place levels are named.
    /// Returns a message listing the valid levels on an unknown value — no
    /// silent fallback (matches agent-env validation).
    pub fn from_input(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        let normalized = trimmed.to_ascii_lowercase();
        Self::ALL
            .iter()
            .find(|level| level.as_str() == normalized)
            .cloned()
            .ok_or_else(|| {
                let valid = Self::ALL
                    .iter()
                    .map(|l| l.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("'{trimmed}' invalid (valid: {valid})")
            })
    }
}

#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub error_message: Option<String>,
    /// True when the error is a context-length-exceeded rejection (the
    /// prompt no longer fits the model's window). Classified by each
    /// provider at its own boundary — per ADR-001 no provider phrasing
    /// crosses into core — so the compaction reactive path (ADR-002) can
    /// know that compacting and retrying will help.
    pub context_overflow: bool,
    /// True when the failure looks *transient* — a rate limit, overload, or
    /// upstream/network fault — so retrying the same request may succeed
    /// (vikunja #1240). Classified by each provider at its own boundary for the
    /// same reason as `context_overflow`: per ADR-001 no provider phrasing
    /// crosses into core, so core decides *whether* to retry but never *what
    /// counts* as retryable.
    ///
    /// Independent of `context_overflow`: an oversized prompt is fatal for
    /// retry purposes (ADR-002's reactive compaction handles it, and resending
    /// the same prompt would fail identically).
    pub retryable: bool,
    pub usage: Usage,
}

impl LlmResponse {
    pub fn error(msg: impl Into<String>) -> Self {
        LlmResponse {
            content: vec![],
            stop_reason: StopReason::Error,
            error_message: Some(msg.into()),
            context_overflow: false,
            // Unclassified errors (parse failures, unknown response shapes) are
            // deliberately NOT retryable: retrying a deterministic failure just
            // burns tokens and latency.
            retryable: false,
            usage: Usage::default(),
        }
    }

    /// An error response classified as a context-window overflow.
    pub fn context_overflow_error(msg: impl Into<String>) -> Self {
        LlmResponse {
            context_overflow: true,
            ..Self::error(msg)
        }
    }

    /// An error response classified as transient — safe to retry (#1240).
    pub fn retryable_error(msg: impl Into<String>) -> Self {
        LlmResponse {
            retryable: true,
            ..Self::error(msg)
        }
    }
}

pub const MISSING_STRUCTURED_TOOL_CALL: &str =
    "provider declared tool use but returned no structured tool call";

pub fn tool_use_missing_structured_call(response: &LlmResponse) -> bool {
    response.stop_reason == StopReason::ToolUse
        && !response
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall { .. }))
}

/// Enforce the provider-neutral tool-use response invariant.
///
/// A `ToolUse` stop without a `ToolCall` cannot be dispatched or paired with a
/// user-role `ToolResult`. Continuing would leave assistant text at the end of
/// history, which Anthropic-compatible routes interpret as unsupported
/// assistant prefill. Preserve accounting from the malformed attempt, but
/// clear its pseudo-tool content and fail before it enters durable history.
pub fn validate_response_shape(mut response: LlmResponse) -> LlmResponse {
    if tool_use_missing_structured_call(&response) {
        response.content.clear();
        response.stop_reason = StopReason::Error;
        response
            .error_message
            .get_or_insert_with(|| MISSING_STRUCTURED_TOOL_CALL.to_string());
        response.context_overflow = false;
        response.retryable = false;
    }
    response
}

/// Resolved provider deadlines, in the form the adapters need (vikunja #1107).
///
/// Lives in the provider layer and is shared by all three adapters for the same
/// reason as [`is_retryable_status`]: these are transport properties, not
/// provider phrasing, so one implementation cannot drift into three.
#[derive(Debug, Clone, Copy)]
pub struct ProviderTimeouts {
    pub connect: std::time::Duration,
    pub headers: std::time::Duration,
    pub first_event: std::time::Duration,
    pub idle: std::time::Duration,
    /// `None` when no hard generation ceiling is configured.
    pub generation: Option<std::time::Duration>,
}

impl Default for ProviderTimeouts {
    fn default() -> Self {
        Self::from_config(&crate::config::ProviderTimeoutConfig::default())
    }
}

impl ProviderTimeouts {
    pub fn from_config(cfg: &crate::config::ProviderTimeoutConfig) -> Self {
        let secs = std::time::Duration::from_secs;
        Self {
            connect: secs(cfg.connect_secs),
            headers: secs(cfg.headers_secs),
            first_event: secs(cfg.first_event_secs),
            idle: secs(cfg.idle_secs),
            generation: (cfg.generation_secs > 0).then(|| secs(cfg.generation_secs)),
        }
    }

    /// An HTTP client carrying the connect deadline. Every adapter builds its
    /// client through here so none can be constructed without one.
    pub fn client(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .connect_timeout(self.connect)
            .build()
            // A builder failure here means TLS backend init failed; the
            // default client is still better than refusing to run, and the
            // request-level deadlines below still apply.
            .unwrap_or_default()
    }
}

/// Await `fut` with a deadline, mapping expiry to a **retryable** provider
/// error (#1107 + #1240).
///
/// Retryable is the correct classification: a stalled socket is transient, so
/// the retry/failover path added in #1240 will re-issue the request or move to
/// the next model instead of surfacing a dead turn to the user. The message
/// names only the phase and the limit — never the request body, headers or key.
pub async fn with_deadline<T>(
    phase: &str,
    limit: std::time::Duration,
    fut: impl std::future::Future<Output = T>,
) -> Result<T, LlmResponse> {
    match tokio::time::timeout(limit, fut).await {
        Ok(value) => Ok(value),
        Err(_) => Err(LlmResponse::retryable_error(format!(
            "provider {phase} timed out after {}s",
            limit.as_secs()
        ))),
    }
}

/// Whether an HTTP status from a provider indicates a *transient* failure worth
/// retrying (vikunja #1240).
///
/// Lives in the provider layer and is shared by all three adapters: HTTP status
/// semantics are a standard rather than provider phrasing, so classifying here
/// respects ADR-001 while avoiding a triplicated table that would drift.
///
/// Retryable: 408 request timeout, 429 rate limit, and any 5xx — including
/// Cloudflare's 52x family, which providers behind a CDN return during
/// overload. Everything else is fatal: 4xx means the request itself is wrong,
/// so retrying it — or failing over to another model with the same bad request
/// — would only mask the bug.
pub fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 429) || (500..600).contains(&status)
}

/// Classify a mid-stream SSE read failure and build the provider error response
/// (vikunja #1418).
///
/// This is reached only *after* a successful 2xx header response, once the event
/// stream is established (a bad request would already have failed at the status
/// check). An [`EventStreamError::Transport`] fault is therefore the socket
/// breaking under an open stream — an HTTP/2 `CANCEL`, a connection reset, an
/// abrupt idle disconnect — which is transient: re-issuing the request usually
/// succeeds, so core's #1240 retry/failover/resume path should absorb it. A
/// `Utf8` or `Parser` fault is the server emitting bytes we cannot decode;
/// re-issuing the same request yields the same undecodable bytes, so it is fatal.
///
/// This classifies only the *transport* framing of the SSE stream. An in-band
/// error the server sends as a well-formed SSE `error` event (e.g. OpenAI's
/// `response.error`) is a decoded payload, not a transport fault, and must be
/// classified by the adapter on its own terms — do not funnel such events here.
///
/// Lives in the provider layer for the same ADR-001 reason as
/// [`is_retryable_status`]: SSE framing is a standard, not provider phrasing, so
/// one classifier serves every adapter instead of three tables that could drift.
/// The message names only the provider prefix and the transport phrasing — never
/// the request body, headers, or key.
pub fn stream_error_response<E: std::fmt::Display>(
    prefix: &str,
    error: eventsource_stream::EventStreamError<E>,
) -> LlmResponse {
    use eventsource_stream::EventStreamError;
    let message = format!("{prefix}: {error}");
    match error {
        EventStreamError::Transport(_) => LlmResponse::retryable_error(message),
        EventStreamError::Utf8(_) | EventStreamError::Parser(_) => LlmResponse::error(message),
    }
}

pub struct Context {
    pub messages: Vec<Message>,
    pub system: Option<String>,
    pub tools: Vec<ToolSchema>,
    /// Index into `messages` at which the stable prefix ends.
    /// Provider places its cache_control breakpoint at this boundary.
    pub stable_prefix_len: usize,
}

#[derive(Clone)]
pub struct CompleteOpts {
    pub model: String,
    pub max_tokens: u32,
    pub thinking: ThinkingLevel,
    /// Sampling temperature. `None` (default) sends no sampling params at
    /// all — the provider/model default applies. Set (e.g. `Some(0.0)`) by
    /// the compaction summarizer for deterministic summaries (ADR-002).
    pub temperature: Option<f64>,
}

/// The sentinel output budget in [`CompleteOpts::default`]. A caller that
/// leaves `max_tokens` at this value is treated as "unset": each provider
/// substitutes the model's real maximum output (see [`resolve_max_output`]),
/// because 8192 is far too small once adaptive reasoning draws from the same
/// budget and truncates a fresh turn with `stop_reason: max_tokens` — which
/// Zed surfaces to the user as "Output Limit Reached".
pub const DEFAULT_MAX_TOKENS: u32 = 8192;

impl Default for CompleteOpts {
    fn default() -> Self {
        CompleteOpts {
            model: "claude-opus-4-8".to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
            thinking: ThinkingLevel::default(),
            temperature: None,
        }
    }
}

/// Resolve the output-token budget a provider actually sends for a model.
///
/// The struct default of [`DEFAULT_MAX_TOKENS`] (8192) is a placeholder, not a
/// real intent: production callers build `CompleteOpts { model, thinking,
/// ..default() }` and never set `max_tokens`, so every fresh session inherits
/// 8192. With adaptive reasoning on (the default), thinking tokens are drawn
/// from that same budget, so a hard first prompt can exhaust it before the
/// visible answer and truncate with `stop_reason: max_tokens`.
///
/// Resolution:
/// - `max_tokens` left at the [`DEFAULT_MAX_TOKENS`] sentinel → substitute
///   `DAIMONOS_AGENT_MAX_TOKENS` if set, else `model_default` (the model's real
///   max output), else leave it unchanged for models we have no figure for.
/// - an explicit, non-sentinel `max_tokens` is the caller's deliberate choice
///   and is honored as-is.
/// - `hard_cap`, when the model has a known ceiling, clamps the result so we
///   never request more output than the model allows.
pub(crate) fn resolve_max_output(
    requested: u32,
    model_default: Option<u32>,
    hard_cap: Option<u32>,
) -> u32 {
    resolve_max_output_inner(
        requested,
        model_default,
        hard_cap,
        max_tokens_env_override(),
    )
}

fn resolve_max_output_inner(
    requested: u32,
    model_default: Option<u32>,
    hard_cap: Option<u32>,
    env_override: Option<u32>,
) -> u32 {
    let base = if requested == DEFAULT_MAX_TOKENS {
        env_override.or(model_default).unwrap_or(requested)
    } else {
        requested
    };
    match hard_cap {
        Some(cap) => base.min(cap),
        None => base,
    }
}

/// `DAIMONOS_AGENT_MAX_TOKENS`, a positive integer, lets an operator raise or
/// cap the default output budget without a code change. Ignored when empty,
/// non-numeric, or zero.
fn max_tokens_env_override() -> Option<u32> {
    std::env::var("DAIMONOS_AGENT_MAX_TOKENS")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|&v| v > 0)
}

/// An incremental piece of a turn, emitted while a provider streams a
/// response. Only text/thinking are streamed live today — tool-call inputs
/// are still accumulated atomically into the final `LlmResponse`, since a
/// terminal REPL has no use for partial JSON arguments.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    ThinkingDelta(String),
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn complete(&self, ctx: &Context, opts: &CompleteOpts) -> LlmResponse;

    /// Whether this provider adapter can serialize image prompt blocks.
    /// Defaults false so test doubles and text-only providers never cause ACP
    /// clients to send content that would be discarded.
    fn supports_images(&self) -> bool {
        false
    }

    /// Like `complete`, but invokes `on_event` with each `StreamEvent` as it
    /// arrives, before returning the same final `LlmResponse` `complete`
    /// would produce. Default: delegates to `complete`, then projects its final
    /// text/thinking blocks as complete deltas so conforming non-streaming
    /// providers still produce the canonical frontend transcript.
    async fn stream(
        &self,
        ctx: &Context,
        opts: &CompleteOpts,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> LlmResponse {
        let response = self.complete(ctx, opts).await;
        for block in &response.content {
            match block {
                ContentBlock::Text(text) => on_event(StreamEvent::TextDelta(text.clone())),
                ContentBlock::Thinking(text) => {
                    on_event(StreamEvent::ThinkingDelta(text.clone()));
                }
                ContentBlock::Image { .. }
                | ContentBlock::ProviderState { .. }
                | ContentBlock::ToolCall { .. }
                | ContentBlock::ToolResult { .. } => {}
            }
        }
        response
    }

    /// The maximum input/context window, in tokens, the provider reports for
    /// `model` — used to resolve `DAIMONOS_AGENT_CONTEXT_WINDOW` when the
    /// agent env file omits it (ADR-002 amendment, vikunja #965). Default
    /// `None`, so test doubles and providers that can't answer are treated as
    /// "no live value available". Implementations return `None` on any
    /// failure (network error, unknown model id, missing/zero field) rather
    /// than guessing — the caller then hard-errors telling the user to set
    /// the key explicitly.
    async fn context_window(&self, _model: &str) -> Option<u64> {
        None
    }

    /// Live model catalog from the provider's list-models endpoint, newest
    /// first where the API exposes recency. `None` means "no live catalog"
    /// (unsupported adapter, network/auth failure, or an empty list) — the
    /// caller falls back to the statically configured model list rather than
    /// guessing. Feeds startup model discovery: the `/model` picker and the
    /// #1240 failover chain.
    async fn list_models(&self) -> Option<Vec<String>> {
        None
    }

    /// The thinking levels this adapter sends distinctly for `model`, in
    /// ascending-effort order. Feeds per-session reasoning selectors, which
    /// must not offer a level the provider ignores or collapses into another.
    /// `None` means no truthful selector can be offered (unknown model, or an
    /// adapter that does not distinguish levels).
    async fn thinking_levels(&self, _model: &str) -> Option<Vec<ThinkingLevel>> {
        None
    }
}

// === WP07: controlled model transport (ADR-025, FR-04 requirements 1-13) ===
//
// Context: docs/contexts/reviewer-runtime/CONTEXT.md; ADR-025. An opt-in,
// profile-scoped request transport that leaves the legacy adapters unchanged.
// These are the provider-NEUTRAL option seams (integrator-owned per the G0
// ledger). All vendor wire rules stay provider-local in anthropic.rs (Messages)
// and openrouter.rs (Chat); this module only resolves the caller-owned profile,
// builds the profile-scoped HTTP client, and runs the shared attempt/redirect
// discipline. No field is inferred and no hidden default is supplied.

/// The two serialization wires the controlled profile supports. Never inferred
/// from a model name or base URL (FR-04 requirement 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// Chat Completions (`{base}/chat/completions`), including OpenRouter's
    /// OpenAI-compatible endpoint.
    Chat,
    /// Anthropic Messages (`{base}/v1/messages`, stable path only).
    Messages,
}

impl Wire {
    pub fn parse(value: &str) -> Option<Wire> {
        match value {
            "chat" => Some(Wire::Chat),
            "messages" => Some(Wire::Messages),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Wire::Chat => "chat",
            Wire::Messages => "messages",
        }
    }
}

/// The five caller-owned values that fully identify a controlled model request
/// (FR-04 requirements 1-2). All are required and come from ONE source; none is
/// combined with an env/flag fallback. The credential VALUE is read elsewhere
/// from `credential_env` and never stored here, exported, or logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlledModelSpec {
    pub provider_label: String,
    pub model: String,
    pub base_url: String,
    pub wire: Wire,
    pub credential_env: String,
}

/// Caller-owned controlled request settings beyond the five identity values
/// (FR-04 requirements 6, 10, 11, 13). `extra_body_fields` are explicit, audited
/// additions the caller configured; they can never overwrite a protected field
/// or introduce a forbidden sampling/forced-choice/strict behavior.
#[derive(Debug, Clone)]
pub struct ControlledRequest {
    /// Caller-owned output ceiling (FR-04 req 6). There is no built-in default
    /// on the request path: the Messages wire requires it (enforced at startup
    /// in [`ControlledProfile::from_json`]); the Chat wire omits the field
    /// entirely when the caller did not set it.
    pub max_tokens: Option<u32>,
    pub timeout: std::time::Duration,
    pub request_retries: u32,
    pub proxy: Option<String>,
    pub extra_body_fields: serde_json::Map<String, Value>,
}

impl Default for ControlledRequest {
    fn default() -> Self {
        ControlledRequest {
            max_tokens: None,
            timeout: std::time::Duration::from_secs(60),
            request_retries: 0,
            proxy: None,
            extra_body_fields: serde_json::Map::new(),
        }
    }
}

/// A fully prepared controlled HTTP request. The wire adapter owns the URL, the
/// exact fixed header set (credential header included), and the fixed body; the
/// shared executor only sends it. Keeping this provider-built preserves
/// wire-local ownership while sharing the no-redirect/no-ambient-proxy client.
pub struct ControlledHttp {
    pub url: String,
    /// Exact, ordered header set. The credential value is already injected and
    /// must never be logged.
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

/// The fixed User-Agent identifying Daimonos and its version (FR-04 req 8).
pub fn controlled_user_agent() -> String {
    format!("daimonos/{}", env!("CARGO_PKG_VERSION"))
}

/// The complete parsed controlled profile: the five identity values plus the
/// request settings. Built from one JSON object; see [`ControlledProfile::from_json`].
#[derive(Debug, Clone)]
pub struct ControlledProfile {
    pub spec: ControlledModelSpec,
    pub request: ControlledRequest,
}

/// The model-request retry ceiling. ADR-025 ("Controlled model requests use an
/// isolated opt-in transport profile") specifies that the controlled transport
/// owns the exact attempt count and that a caller-configured `request_retries`
/// is subject to the model-request ceiling. ADR-025 treats this ceiling as a
/// safety invariant of the transport, NOT an operator setting, so it is a
/// compile-time constant rather than a `daimonos.default.toml` tunable; a
/// profile naming a larger value is refused at startup validation.
pub const CONTROLLED_REQUEST_RETRY_CEILING: u32 = 100;

impl ControlledProfile {
    /// Parse and validate one complete profile object. Fails naming every
    /// missing required value (FR-04 req 2), rejects an inferred/invalid wire
    /// (req 3), and refuses any unconfigured extra top-level field. Reading the
    /// credential value is NOT performed here: this validates configuration
    /// only, so a partial profile fails before any network or credential read.
    pub fn from_json(value: &Value) -> Result<ControlledProfile, String> {
        let obj = value
            .as_object()
            .ok_or_else(|| "model profile must be a JSON object".to_string())?;

        const IDENTITY: [&str; 5] = [
            "provider_label",
            "model",
            "base_url",
            "wire",
            "credential_env",
        ];
        const OPTIONAL: [&str; 5] = [
            "max_tokens",
            "timeout_ms",
            "request_retries",
            "proxy",
            "extra_body_fields",
        ];

        // Refuse any field we do not explicitly understand: an unconfigured
        // extra field must fail, never travel silently (FR-04 req 6, defaults).
        let unknown: Vec<&str> = obj
            .keys()
            .map(String::as_str)
            .filter(|k| !IDENTITY.contains(k) && !OPTIONAL.contains(k))
            .collect();
        if !unknown.is_empty() {
            return Err(format!(
                "model profile has unconfigured field(s): {}",
                unknown.join(", ")
            ));
        }

        let string_field = |name: &str| -> Option<String> {
            obj.get(name)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };

        let mut missing: Vec<&str> = Vec::new();
        let provider_label = string_field("provider_label");
        let model = string_field("model");
        let base_url = string_field("base_url");
        let wire_raw = string_field("wire");
        let credential_env = string_field("credential_env");
        for (present, name) in [
            (provider_label.is_some(), "provider_label"),
            (model.is_some(), "model"),
            (base_url.is_some(), "base_url"),
            (wire_raw.is_some(), "wire"),
            (credential_env.is_some(), "credential_env"),
        ] {
            if !present {
                missing.push(name);
            }
        }
        if !missing.is_empty() {
            return Err(format!(
                "model profile incomplete: missing {}",
                missing.join(", ")
            ));
        }

        let wire_raw = wire_raw.unwrap();
        let wire = Wire::parse(&wire_raw).ok_or_else(|| {
            format!("model profile wire must be \"chat\" or \"messages\", not {wire_raw:?}")
        })?;

        let mut request = ControlledRequest::default();
        if let Some(v) = obj.get("max_tokens") {
            request.max_tokens = Some(
                v.as_u64()
                    .filter(|n| *n > 0 && *n <= u32::MAX as u64)
                    .ok_or_else(|| {
                        "model profile max_tokens must be a positive integer".to_string()
                    })? as u32,
            );
        }
        // max_tokens is caller-owned (FR-04 req 6). On the Messages wire the
        // field is mandatory, so a Messages profile that omits it fails at
        // startup naming the field — no built-in default travels on the wire.
        if wire == Wire::Messages && request.max_tokens.is_none() {
            return Err(
                "model profile incomplete: the messages wire requires max_tokens".to_string(),
            );
        }
        if let Some(v) = obj.get("timeout_ms") {
            let ms = v
                .as_u64()
                .filter(|n| *n > 0)
                .ok_or_else(|| "model profile timeout_ms must be a positive integer".to_string())?;
            request.timeout = std::time::Duration::from_millis(ms);
        }
        if let Some(v) = obj.get("request_retries") {
            request.request_retries = v
                .as_u64()
                .filter(|n| *n <= CONTROLLED_REQUEST_RETRY_CEILING as u64)
                .ok_or_else(|| {
                    format!(
                        "model profile request_retries must be a non-negative integer no greater \
                         than the model-request ceiling of {CONTROLLED_REQUEST_RETRY_CEILING}"
                    )
                })? as u32;
        }
        if let Some(v) = obj.get("proxy") {
            if !v.is_null() {
                request.proxy = Some(
                    v.as_str()
                        .filter(|s| !s.trim().is_empty())
                        .ok_or_else(|| {
                            "model profile proxy must be a non-empty string".to_string()
                        })?
                        .to_string(),
                );
            }
        }
        if let Some(v) = obj.get("extra_body_fields") {
            let map = v
                .as_object()
                .ok_or_else(|| "model profile extra_body_fields must be an object".to_string())?;
            // Refuse a protected-field overwrite or a forbidden
            // sampling/forced-choice/strict/stream/multi-choice key at startup,
            // before any request is built (FR-04 req 4; ADR-025).
            for key in map.keys() {
                if PROTECTED_BODY_FIELDS.contains(&key.as_str()) {
                    return Err(format!(
                        "model profile extra_body_fields may not overwrite protected field {key:?}"
                    ));
                }
                if FORBIDDEN_BODY_FIELDS.contains(&key.as_str()) {
                    return Err(format!(
                        "model profile extra_body_fields may not introduce forbidden field {key:?}"
                    ));
                }
            }
            request.extra_body_fields = map.clone();
        }

        Ok(ControlledProfile {
            spec: ControlledModelSpec {
                provider_label: provider_label.unwrap(),
                model: model.unwrap(),
                base_url: base_url.unwrap(),
                wire,
                credential_env: credential_env.unwrap(),
            },
            request,
        })
    }
}

/// Top-level body fields the caller's `extra_body_fields` may never overwrite,
/// and forbidden sampling/forced-choice/strict keys they may never introduce
/// (FR-04 requirements 4, 5, 6). Applies to both wires.
pub(crate) const PROTECTED_BODY_FIELDS: [&str; 6] = [
    "model",
    "max_tokens",
    "messages",
    "tools",
    "tool_choice",
    "system",
];
/// Sampling, forced-choice, strictness, streaming, and multi-choice parameters
/// the caller's `extra_body_fields` may never introduce on EITHER wire. FR-04
/// req 4 bans "temperature, top_p, top_k, or any other sampling parameter" and
/// ADR-025 states explicit extensions cannot override the mandatory
/// sampling/forced-choice prohibitions. Neither ADR-025 nor INTERFACES.md names
/// an allowlist of permitted extra keys, so a complete denylist across both the
/// Messages and Chat vocabularies is enforced (decision D-WP07-R4-R1). Keys
/// that collide with a field Daimonos sets itself are refused separately via
/// [`PROTECTED_BODY_FIELDS`].
pub(crate) const FORBIDDEN_BODY_FIELDS: [&str; 21] = [
    // sampling
    "temperature",
    "top_p",
    "top_k",
    "min_p",
    "top_a",
    "typical_p",
    "frequency_penalty",
    "presence_penalty",
    "repetition_penalty",
    "seed",
    "logit_bias",
    // multi-choice / forced output detail
    "n",
    "best_of",
    "logprobs",
    "top_logprobs",
    // forced choice
    "function_call",
    "parallel_tool_calls",
    // strictness / structured forcing
    "strict",
    "response_format",
    // streaming
    "stream",
    "stream_options",
];

/// Merge caller `extra_body_fields` into a controlled body. Refuses to overwrite
/// a protected field or introduce a forbidden sampling/forced-choice/strict key.
pub(crate) fn apply_extra_body_fields(
    body: &mut serde_json::Map<String, Value>,
    extra: &serde_json::Map<String, Value>,
) -> Result<(), String> {
    for (key, value) in extra {
        if PROTECTED_BODY_FIELDS.contains(&key.as_str()) {
            return Err(format!(
                "controlled extra_body_fields may not overwrite protected field {key:?}"
            ));
        }
        if FORBIDDEN_BODY_FIELDS.contains(&key.as_str()) {
            return Err(format!(
                "controlled extra_body_fields may not introduce forbidden field {key:?}"
            ));
        }
        body.insert(key.clone(), value.clone());
    }
    Ok(())
}

/// Build the profile-scoped HTTP client (FR-04 requirements 9-13). Explicit
/// caller timeout; no redirects; no ambient proxy (only an explicit one); no
/// default headers; no client-library telemetry. A construction error fails the
/// profile — the legacy fallback client is never reused.
pub fn controlled_client(req: &ControlledRequest) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(req.timeout)
        // Ignore HTTP_PROXY/HTTPS_PROXY/ALL_PROXY and any system proxy.
        .no_proxy();
    if let Some(proxy) = &req.proxy {
        builder = builder.proxy(
            reqwest::Proxy::all(proxy).map_err(|e| format!("controlled proxy invalid: {e}"))?,
        );
    }
    builder
        .build()
        .map_err(|e| format!("controlled client construction failed: {e}"))
}

/// Dispatch one controlled request end to end: build the wire-local request,
/// build the profile-scoped client, and run the shared attempt discipline. The
/// credential value is injected into the wire headers and never logged. Returns
/// an [`LlmResponse`]; a 3xx is a terminal endpoint error (never followed, body
/// never re-sent to another host, FR-04 req 12).
pub async fn controlled_send(
    spec: &ControlledModelSpec,
    req: &ControlledRequest,
    credential: &str,
    ctx: &Context,
) -> LlmResponse {
    let http = match spec.wire {
        Wire::Messages => anthropic::controlled_request(spec, req, credential, ctx),
        Wire::Chat => openrouter::controlled_request(spec, req, credential, ctx),
    };
    let http = match http {
        Ok(h) => h,
        Err(e) => return LlmResponse::error(e),
    };
    let client = match controlled_client(req) {
        Ok(c) => c,
        Err(e) => return LlmResponse::error(e),
    };
    let body = match serde_json::to_string(&http.body) {
        Ok(b) => b,
        Err(e) => return LlmResponse::error(format!("controlled body serialize failed: {e}")),
    };
    let user_agent = controlled_user_agent();
    let max_attempts = req.request_retries.saturating_add(1);
    let mut last = LlmResponse::error("controlled request made no attempt");
    for attempt in 0..max_attempts {
        // Every attempt is observable (FR-04 req 10). The audit record (WP08)
        // consumes the sequence; here we only emit a non-sensitive count.
        tracing::debug!(
            target: "daimonos::providers",
            event = "controlled_request_attempt",
            attempt = attempt + 1,
            of = max_attempts,
            wire = spec.wire.as_str(),
        );
        let mut rb = client.post(&http.url).header("user-agent", &user_agent);
        for (name, value) in &http.headers {
            rb = rb.header(name.as_str(), value.as_str());
        }
        let sent = rb.body(body.clone()).send();
        let result = match with_deadline("controlled response headers", req.timeout, sent).await {
            Ok(r) => r,
            Err(timeout) => {
                last = timeout;
                continue;
            }
        };
        let resp = match result {
            Ok(r) => r,
            Err(e) => {
                // Transport fault: transient, so a caller-configured retry may
                // re-issue it. Message names no body/headers/key. A client-side
                // timeout (the caller's `timeout_ms`) is named explicitly so the
                // failure is observable as a timeout, not a generic send error
                // (FR-04 req 11; R4).
                let msg = if e.is_timeout() {
                    format!(
                        "controlled request timed out after {}ms",
                        req.timeout.as_millis()
                    )
                } else {
                    format!("controlled network error: {e}")
                };
                last = LlmResponse::retryable_error(msg);
                continue;
            }
        };
        let status = resp.status();
        if status.is_redirection() {
            // No redirect is followed and the body is never re-sent to another
            // host; a 3xx is a terminal endpoint error (FR-04 req 12).
            return LlmResponse::error(format!(
                "controlled endpoint error: unexpected redirect {status}"
            ));
        }
        if status.is_success() {
            return controlled_decode(spec, &resp.text().await.unwrap_or_default(), &spec.model);
        }
        let full = format!("controlled endpoint error: API {status}");
        if is_retryable_status(status.as_u16()) {
            last = LlmResponse::retryable_error(full);
            continue;
        }
        return LlmResponse::error(full);
    }
    // The controlled transport owns the EXACT attempt count (FR-04 req 10):
    // the caller's `request_retries` are the sole retry authority, consumed
    // inside this loop. Hand back a terminal (non-retryable) error so the
    // agent loop adds no hidden retry or error-resume of its own on top
    // (AC-04.3). Intermediate `last` values stay retryable only to drive this
    // local loop.
    last.retryable = false;
    last
}

/// Decode a successful controlled response body into an [`LlmResponse`] using
/// the wire-local parser.
fn controlled_decode(spec: &ControlledModelSpec, body: &str, model: &str) -> LlmResponse {
    match spec.wire {
        Wire::Messages => anthropic::controlled_decode(body, model),
        Wire::Chat => openrouter::controlled_decode(body, model),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    pub async fn mock_http_server(
        status: &str,
        content_type: &str,
        response_body: String,
    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let status = status.to_string();
        let content_type = content_type.to_string();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let n = socket.read(&mut buffer).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..n]);
                let text = String::from_utf8_lossy(&request);
                if let Some(split) = text.find("\r\n\r\n") {
                    let length = text[..split]
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(str::trim)
                                .and_then(|value| value.parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= split + 4 + length {
                        break;
                    }
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&request).into_owned());
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        (format!("http://{address}"), rx)
    }

    /// WP07: a fake endpoint that answers EVERY connection with a fixed status
    /// (and optional extra header, e.g. `Location` for a redirect), counting
    /// the connections it accepts. Used to prove attempt counts and that a
    /// non-2xx/redirect produces no extra request and no second host is hit.
    pub async fn counting_http_server(
        status: &'static str,
        extra_header: Option<(&'static str, &'static str)>,
        response_body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let task_count = count.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                task_count.fetch_add(1, Ordering::SeqCst);
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let n = match socket.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    request.extend_from_slice(&buffer[..n]);
                    let text = String::from_utf8_lossy(&request);
                    if let Some(split) = text.find("\r\n\r\n") {
                        let length = text[..split]
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(str::trim)
                                    .and_then(|v| v.parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if request.len() >= split + 4 + length {
                            break;
                        }
                    }
                }
                let extra = extra_header
                    .map(|(k, v)| format!("{k}: {v}\r\n"))
                    .unwrap_or_default();
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        (format!("http://{address}"), count)
    }

    /// WP07 R4: a fake endpoint that accepts the connection, consumes the
    /// request, then sleeps `delay_ms` before replying 200. A controlled
    /// request with a shorter `timeout_ms` must terminate with a timeout error
    /// before the reply ever arrives.
    pub async fn slow_http_server(delay_ms: u64) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buffer = [0u8; 4096];
                let _ = socket.read(&mut buffer).await;
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                let body =
                    r#"{"content":[{"type":"text","text":"late"}],"stop_reason":"end_turn"}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        format!("http://{address}")
    }

    /// WP07 R7 (AC-04.5): a fake Messages endpoint that returns HTTP 400 for any
    /// FORCED `tool_choice` (`type` of `any`/`tool`, or anything other than
    /// `auto`) and 200 otherwise. Because the controlled wire always sends
    /// `{"type":"auto"}`, a controlled request is accepted; the endpoint would
    /// reject a forced choice. Counts (accepted_ok, rejected_400).
    pub async fn forced_choice_rejecting_messages_server(
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let rejected = Arc::new(AtomicUsize::new(0));
        let task_rejected = rejected.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let n = match socket.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    request.extend_from_slice(&buffer[..n]);
                    let text = String::from_utf8_lossy(&request);
                    if let Some(split) = text.find("\r\n\r\n") {
                        let length = text[..split]
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(str::trim)
                                    .and_then(|v| v.parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if request.len() >= split + 4 + length {
                            break;
                        }
                    }
                }
                let raw = String::from_utf8_lossy(&request);
                let body = raw.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
                let choice = serde_json::from_str::<serde_json::Value>(body)
                    .ok()
                    .and_then(|v| v.get("tool_choice").cloned());
                let forced = match &choice {
                    Some(serde_json::Value::Object(o)) => {
                        o.get("type").and_then(serde_json::Value::as_str) != Some("auto")
                    }
                    Some(serde_json::Value::String(s)) => s != "auto",
                    // A tools request with no tool_choice would let the model be
                    // forced by default on some servers; treat absence as forced.
                    _ => true,
                };
                let (status, payload) = if forced {
                    task_rejected.fetch_add(1, Ordering::SeqCst);
                    (
                        "400 Bad Request",
                        r#"{"error":"forced tool_choice refused"}"#,
                    )
                } else {
                    (
                        "200 OK",
                        r#"{"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}"#,
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        (format!("http://{address}"), rejected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_tool_use_shape_is_preserved() {
        let response = LlmResponse {
            content: vec![ContentBlock::ToolCall {
                id: "call-1".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "src/main.rs"}),
            }],
            stop_reason: StopReason::ToolUse,
            error_message: None,
            context_overflow: false,
            retryable: false,
            usage: Usage {
                input: 12,
                ..Usage::default()
            },
        };

        let validated = validate_response_shape(response);

        assert_eq!(validated.stop_reason, StopReason::ToolUse);
        assert_eq!(validated.content.len(), 1);
        assert_eq!(validated.usage.input, 12);
        assert!(validated.error_message.is_none());
    }

    #[test]
    fn mixed_text_and_structured_tool_use_shape_is_preserved() {
        let response = LlmResponse {
            content: vec![
                ContentBlock::Text("I will inspect it.".into()),
                ContentBlock::ToolCall {
                    id: "call-1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "src/main.rs"}),
                },
            ],
            stop_reason: StopReason::ToolUse,
            error_message: None,
            context_overflow: false,
            retryable: false,
            usage: Usage::default(),
        };

        let validated = validate_response_shape(response);

        assert_eq!(validated.stop_reason, StopReason::ToolUse);
        assert_eq!(validated.content.len(), 2);
    }

    #[test]
    fn malformed_tool_use_preserves_existing_provider_error() {
        let response = LlmResponse {
            content: vec![ContentBlock::Text("pseudo tool syntax".into())],
            stop_reason: StopReason::ToolUse,
            error_message: Some("provider-specific detail".into()),
            context_overflow: false,
            retryable: false,
            usage: Usage::default(),
        };

        let validated = validate_response_shape(response);

        assert_eq!(validated.stop_reason, StopReason::Error);
        assert_eq!(
            validated.error_message.as_deref(),
            Some("provider-specific detail")
        );
        assert!(validated.content.is_empty());
    }

    /// vikunja #1240: a transient provider failure must be distinguishable from
    /// a fatal one *at the provider boundary*, so core can retry without ever
    /// reading provider phrasing (ADR-001). Status codes are a standard, not
    /// phrasing, so the classifier is shared by all three adapters rather than
    /// triplicated.
    /// vikunja #1107: a timed-out phase must be classified *retryable*, so the
    /// #1240 retry/failover path re-issues it instead of surfacing a dead turn.
    #[tokio::test]
    async fn a_deadline_expiry_is_a_retryable_provider_error() {
        let never = std::future::pending::<()>();
        let err = with_deadline("first event", std::time::Duration::from_millis(20), never)
            .await
            .expect_err("must time out");
        assert!(err.retryable, "a stalled stream is transient");
        assert_eq!(err.stop_reason, StopReason::Error);
        let msg = err.error_message.unwrap_or_default();
        assert!(msg.contains("first event"), "names the phase that stalled");
        assert!(!err.context_overflow);
    }

    #[tokio::test]
    async fn a_deadline_that_is_met_passes_the_value_through() {
        let ok = with_deadline("headers", std::time::Duration::from_secs(5), async { 7 })
            .await
            .expect("well within the limit");
        assert_eq!(ok, 7);
    }

    #[test]
    fn generation_ceiling_is_opt_in() {
        let d = ProviderTimeouts::default();
        assert!(
            d.generation.is_none(),
            "a healthy long generation must not be killed by wall-clock alone"
        );
        // The idle deadline is what catches a stall, and it must be shorter
        // than the first-event allowance, which covers pre-token reasoning.
        assert!(d.idle < d.first_event);
    }

    #[test]
    fn http_status_classifies_transient_versus_fatal_provider_failures() {
        // Transient: rate limits, overload, gateway and upstream faults.
        for status in [429, 500, 502, 503, 504, 408, 522, 524] {
            assert!(is_retryable_status(status), "{status} should be retryable");
        }
        // Fatal: retrying cannot help, and failover would mask a real bug.
        for status in [400, 401, 403, 404, 405, 409, 413, 422] {
            assert!(
                !is_retryable_status(status),
                "{status} must not be retryable"
            );
        }
    }

    /// vikunja #1418: a mid-stream socket break (HTTP/2 CANCEL, reset, abrupt
    /// idle disconnect) surfaces as a `Transport` fault after a healthy 2xx, and
    /// must be classified retryable so the #1240 path resumes the turn instead
    /// of dead-stopping it.
    #[test]
    fn a_transport_stream_error_is_retryable() {
        use eventsource_stream::EventStreamError;
        let broken = EventStreamError::<String>::Transport(
            "http/2 stream closed with error code CANCEL (0x8)".to_string(),
        );
        let resp = stream_error_response("stream error", broken);
        assert!(resp.retryable, "a broken transport is transient");
        assert!(!resp.context_overflow);
        assert_eq!(resp.stop_reason, StopReason::Error);
        let msg = resp.error_message.unwrap_or_default();
        assert!(msg.contains("stream error"), "names the phase: {msg}");
        assert!(
            msg.contains("CANCEL"),
            "carries the transport phrasing: {msg}"
        );
    }

    /// The other half: an undecodable-bytes fault is deterministic. Re-issuing
    /// the request yields the same bad bytes, so retrying would only burn tokens
    /// — it must stay fatal.
    #[test]
    fn an_undecodable_stream_error_is_fatal() {
        use eventsource_stream::EventStreamError;
        let bad_utf8 = String::from_utf8(vec![0xff, 0xfe]).unwrap_err();
        let resp =
            stream_error_response::<String>("stream error", EventStreamError::Utf8(bad_utf8));
        assert!(
            !resp.retryable,
            "malformed bytes cannot be fixed by a retry"
        );
        assert_eq!(resp.stop_reason, StopReason::Error);
    }

    #[test]
    fn retryable_error_marks_the_response_and_leaves_overflow_alone() {
        let r = LlmResponse::retryable_error("API 503: overloaded");
        assert!(r.retryable);
        assert!(!r.context_overflow);
        assert_eq!(r.stop_reason, StopReason::Error);

        // The two classifications are independent: a context overflow is fatal
        // for retry purposes — ADR-002's reactive compaction path handles it,
        // and retrying the same oversized prompt would just fail again.
        let o = LlmResponse::context_overflow_error("prompt is too long");
        assert!(o.context_overflow);
        assert!(!o.retryable);

        // Default construction stays non-retryable so unclassified errors
        // (parse failures, unknown shapes) never trigger a retry loop.
        assert!(!LlmResponse::error("parse error").retryable);
    }

    #[test]
    fn thinking_level_parses_every_known_level_case_insensitively() {
        let cases = [
            ("off", ThinkingLevel::Off),
            ("minimal", ThinkingLevel::Minimal),
            ("low", ThinkingLevel::Low),
            ("medium", ThinkingLevel::Medium),
            ("high", ThinkingLevel::High),
            ("xhigh", ThinkingLevel::XHigh),
            ("max", ThinkingLevel::Max),
        ];
        for (input, expected) in cases {
            assert_eq!(ThinkingLevel::from_input(input), Ok(expected.clone()));
            // Case- and whitespace-insensitive, and round-trips through as_str.
            assert_eq!(
                ThinkingLevel::from_input(&format!("  {}  ", input.to_uppercase())),
                Ok(expected.clone())
            );
            assert_eq!(ThinkingLevel::from_input(expected.as_str()), Ok(expected));
        }
    }

    #[test]
    fn thinking_level_rejects_unknown_value() {
        let err = ThinkingLevel::from_input("turbo").unwrap_err();
        assert!(err.contains("turbo"), "{err}");
        // Error should list the valid levels so the message is actionable.
        assert!(err.contains("high"), "{err}");
    }

    struct StaticProvider(LlmResponse);

    #[async_trait]
    impl LlmProvider for StaticProvider {
        async fn complete(&self, _ctx: &Context, _opts: &CompleteOpts) -> LlmResponse {
            self.0.clone()
        }
    }

    fn dummy_ctx() -> Context {
        Context {
            messages: vec![],
            system: None,
            tools: vec![],
            stable_prefix_len: 0,
        }
    }

    #[tokio::test]
    async fn default_stream_delegates_to_complete() {
        let provider = StaticProvider(LlmResponse {
            retryable: false,
            content: vec![ContentBlock::Text("hi".into())],
            stop_reason: StopReason::EndTurn,
            error_message: None,
            context_overflow: false,
            usage: Usage::default(),
        });
        let mut events = Vec::new();
        let resp = provider
            .stream(&dummy_ctx(), &CompleteOpts::default(), &mut |e| {
                events.push(e)
            })
            .await;
        assert!(matches!(&resp.content[0], ContentBlock::Text(t) if t == "hi"));
        assert_eq!(events, vec![StreamEvent::TextDelta("hi".to_string())]);
    }

    #[tokio::test]
    async fn default_context_window_is_none() {
        let provider = StaticProvider(LlmResponse::error("unused"));
        assert_eq!(provider.context_window("any-model").await, None);
    }

    #[test]
    fn default_provider_does_not_advertise_image_support() {
        let provider = StaticProvider(LlmResponse::error("unused"));
        assert!(!provider.supports_images());
    }

    #[test]
    fn llm_response_error_sets_fields() {
        let r = LlmResponse::error("something went wrong");
        assert_eq!(r.stop_reason, StopReason::Error);
        assert_eq!(r.error_message.as_deref(), Some("something went wrong"));
        assert!(r.content.is_empty());
    }

    #[test]
    fn llm_response_error_usage_is_zero() {
        let r = LlmResponse::error("fail");
        assert_eq!(r.usage.input, 0);
        assert_eq!(r.usage.output, 0);
        assert_eq!(r.usage.cost.total_usd, 0.0);
    }

    #[test]
    fn usage_default_is_zero() {
        let u = Usage::default();
        assert_eq!(u.input, 0);
        assert_eq!(u.output, 0);
        assert_eq!(u.reasoning_output, None);
        assert_eq!(u.thinking_bytes, 0);
        assert_eq!(u.cache_read, 0);
        assert_eq!(u.cache_write, 0);
        assert_eq!(u.cost.total_usd, 0.0);
    }

    #[test]
    fn usage_prompt_tokens_sums_all_window_occupants() {
        // Canonical semantics: input is non-cached; cached tokens still
        // occupy the window, so prompt_tokens() counts all three.
        let u = Usage {
            input: 100,
            output: 9,
            reasoning_output: Some(4),
            thinking_bytes: 9,
            cache_read: 30,
            cache_write: 20,
            cost: Cost::default(),
        };
        assert_eq!(u.prompt_tokens(), 150);
        assert_eq!(Usage::default().prompt_tokens(), 0);
    }

    #[test]
    fn error_constructor_is_not_context_overflow() {
        assert!(!LlmResponse::error("rate limited").context_overflow);
    }

    #[test]
    fn context_overflow_error_constructor_sets_flag() {
        let r = LlmResponse::context_overflow_error("prompt is too long");
        assert!(r.context_overflow);
        assert_eq!(r.stop_reason, StopReason::Error);
        assert_eq!(r.error_message.as_deref(), Some("prompt is too long"));
        assert!(r.content.is_empty());
    }

    #[test]
    fn thinking_level_default_is_medium() {
        assert_eq!(ThinkingLevel::default(), ThinkingLevel::Medium);
    }

    #[test]
    fn complete_opts_default_is_opus_48() {
        let opts = CompleteOpts::default();
        assert_eq!(opts.model, "claude-opus-4-8");
        assert_eq!(opts.max_tokens, 8192);
        assert_eq!(opts.thinking, ThinkingLevel::Medium);
    }

    #[test]
    fn sentinel_with_no_model_default_passes_through() {
        // Unknown model, no env override: the 8192 sentinel is left as-is.
        assert_eq!(
            resolve_max_output_inner(DEFAULT_MAX_TOKENS, None, None, None),
            DEFAULT_MAX_TOKENS
        );
    }

    #[test]
    fn sentinel_substitutes_model_default() {
        // The reported bug: a fresh session's 8192 becomes the model max.
        assert_eq!(
            resolve_max_output_inner(DEFAULT_MAX_TOKENS, Some(32_000), None, None),
            32_000
        );
    }

    #[test]
    fn env_override_beats_model_default_on_sentinel() {
        assert_eq!(
            resolve_max_output_inner(DEFAULT_MAX_TOKENS, Some(32_000), None, Some(50_000)),
            50_000
        );
    }

    #[test]
    fn explicit_value_is_honored_over_default_and_env() {
        // A deliberate, non-sentinel caller value wins; env only fills the sentinel.
        assert_eq!(
            resolve_max_output_inner(4_096, Some(64_000), None, Some(50_000)),
            4_096
        );
    }

    #[test]
    fn hard_cap_clamps_explicit_and_env() {
        // Never request more output than the model ceiling.
        assert_eq!(
            resolve_max_output_inner(u32::MAX, Some(128_000), Some(128_000), None),
            128_000
        );
        assert_eq!(
            resolve_max_output_inner(
                DEFAULT_MAX_TOKENS,
                Some(128_000),
                Some(128_000),
                Some(999_999)
            ),
            128_000
        );
    }

    #[test]
    fn message_user_helper() {
        let m = Message::user("hello");
        assert_eq!(m.role, Role::User);
        assert!(matches!(&m.content[0], ContentBlock::Text(t) if t == "hello"));
    }

    #[test]
    fn provider_state_round_trips_through_session_serialization() {
        let message = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ProviderState {
                provider: "openai".into(),
                data: serde_json::json!({
                    "type": "reasoning",
                    "id": "r1",
                    "encrypted_content": "opaque"
                }),
            }],
        };
        let encoded = serde_json::to_string(&message).unwrap();
        let decoded: Message = serde_json::from_str(&encoded).unwrap();
        assert!(matches!(
            &decoded.content[0],
            ContentBlock::ProviderState { provider, data }
                if provider == "openai" && data["encrypted_content"] == "opaque"
        ));
    }

    #[test]
    fn message_assistant_helper() {
        let m = Message::assistant("hi");
        assert_eq!(m.role, Role::Assistant);
        assert!(matches!(&m.content[0], ContentBlock::Text(t) if t == "hi"));
    }

    #[test]
    fn stop_reason_as_str_is_snake_case() {
        assert_eq!(StopReason::EndTurn.as_str(), "end_turn");
        assert_eq!(StopReason::ToolUse.as_str(), "tool_use");
        assert_eq!(StopReason::Error.as_str(), "error");
        assert_eq!(StopReason::Refusal.as_str(), "refusal");
        assert_eq!(StopReason::Aborted.as_str(), "aborted");
        assert_eq!(StopReason::MaxTokens.as_str(), "max_tokens");
    }

    #[test]
    fn stop_reason_variants_are_distinct() {
        let variants = [
            StopReason::EndTurn,
            StopReason::ToolUse,
            StopReason::Error,
            StopReason::Refusal,
            StopReason::Aborted,
            StopReason::MaxTokens,
        ];
        for (i, a) in variants.iter().enumerate() {
            for (j, b) in variants.iter().enumerate() {
                if i == j {
                    assert_eq!(a, b);
                } else {
                    assert_ne!(a, b);
                }
            }
        }
    }
}

#[cfg(test)]
mod controlled_transport_tests {
    //! WP07 controlled transport (ADR-025). Transport-level AC-04.1-04.5 and
    //! the profile validation behind AC-04.7, against local fake endpoints only.
    use super::test_support::{
        counting_http_server, forced_choice_rejecting_messages_server, mock_http_server,
        slow_http_server,
    };
    use super::*;
    use std::collections::{BTreeSet, HashMap};
    use std::sync::atomic::Ordering;

    fn ctx_with_tool() -> Context {
        Context {
            messages: vec![Message::user("inspect this")],
            system: Some("system prompt".into()),
            tools: vec![ToolSchema {
                name: "final_answer".into(),
                description: "the final answer tool".into(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            }],
            stable_prefix_len: 0,
        }
    }

    fn messages_spec(base_url: String) -> ControlledModelSpec {
        ControlledModelSpec {
            provider_label: "anthropic".into(),
            model: "claude-opus-5-5".into(),
            base_url,
            wire: Wire::Messages,
            credential_env: "CTRL_KEY".into(),
        }
    }

    fn chat_spec(base_url: String) -> ControlledModelSpec {
        ControlledModelSpec {
            provider_label: "openrouter".into(),
            model: "some/model".into(),
            base_url,
            wire: Wire::Chat,
            credential_env: "CTRL_KEY".into(),
        }
    }

    /// Split a captured raw HTTP request into (request_line, lowercased headers, body).
    fn parse_request(raw: &str) -> (String, HashMap<String, String>, String) {
        let (head, body) = raw.split_once("\r\n\r\n").unwrap();
        let mut lines = head.lines();
        let request_line = lines.next().unwrap().to_string();
        let headers = lines
            .filter_map(|l| {
                l.split_once(':')
                    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            })
            .collect();
        (request_line, headers, body.to_string())
    }

    const ANTHROPIC_ALLOWED: [&str; 9] = [
        "x-api-key",
        "anthropic-version",
        "content-type",
        "user-agent",
        "host",
        "content-length",
        "accept",
        "accept-encoding",
        "connection",
    ];
    /// R6 (review r3): a scoped environment variable that is restored (or
    /// removed) when the guard drops, even on panic. Keeps a test that must set
    /// an env var (e.g. to resolve a credential) from leaking it into siblings.
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::set_var(key, value);
            EnvGuard { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    const CHAT_ALLOWED: [&str; 8] = [
        "authorization",
        "content-type",
        "user-agent",
        "host",
        "content-length",
        "accept",
        "accept-encoding",
        "connection",
    ];

    // AC-04.1 (Messages body) + AC-04.5 (automatic, never forced tool choice).
    #[tokio::test]
    async fn controlled_messages_body_and_headers_are_the_fixed_set() {
        let (base, rx) = mock_http_server(
            "200 OK",
            "application/json",
            r#"{"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}"#.into(),
        )
        .await;
        let resp = controlled_send(
            &messages_spec(base),
            &ControlledRequest {
                max_tokens: Some(1024),
                ..ControlledRequest::default()
            },
            "SECRET_CREDENTIAL",
            &ctx_with_tool(),
        )
        .await;
        assert!(resp.error_message.is_none(), "{:?}", resp.error_message);

        let raw = rx.await.unwrap();
        let (request_line, headers, body) = parse_request(&raw);
        // Stable path only; never a beta path or query string (req 7).
        assert!(
            request_line.starts_with("POST /v1/messages "),
            "{request_line}"
        );
        assert!(!request_line.contains("beta"));
        assert_eq!(headers.get("x-api-key").unwrap(), "SECRET_CREDENTIAL");
        assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
        assert!(headers.get("user-agent").unwrap().starts_with("daimonos/"));
        for name in headers.keys() {
            assert!(
                ANTHROPIC_ALLOWED.contains(&name.as_str()),
                "unexpected header {name}"
            );
        }

        let obj = serde_json::from_str::<Value>(&body).unwrap();
        let obj = obj.as_object().unwrap();
        let keys: BTreeSet<&str> = obj.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "max_tokens",
                "messages",
                "model",
                "system",
                "tool_choice",
                "tools"
            ]
            .into_iter()
            .collect::<BTreeSet<&str>>()
        );
        assert_eq!(obj["model"], Value::from("claude-opus-5-5"));
        assert_eq!(obj["max_tokens"], Value::from(1024));
        // Automatic, never forced (req 5).
        assert_eq!(obj["tool_choice"], serde_json::json!({"type": "auto"}));
        for forced in ["required", "any", "tool"] {
            assert_ne!(obj["tool_choice"]["type"], Value::from(forced));
        }
        // No sampling, no stream, no strict flag anywhere (req 4, 5).
        for banned in ["temperature", "top_p", "top_k", "stream"] {
            assert!(obj.get(banned).is_none(), "sampling field {banned} present");
        }
        assert!(obj["tools"][0].get("strict").is_none());
        assert!(!body.contains("cache_control"));
    }

    // AC-04.2 (Messages): the request carries ONLY the fixed allowed header
    // set. R6 (review r3): this no longer mutates process-global env (which the
    // controlled path never reads, so it proved nothing and leaked into sibling
    // tests). Ambient org/project/custom-header and proxy env vars are proven
    // ignored at the PROCESS level in tests/test_request_control.py, where the
    // subprocess environment is isolated. Here the fixed header set is enforced
    // by construction: the adapter emits an exact header vec and the client has
    // no default headers or middleware.
    #[tokio::test]
    async fn controlled_messages_emits_only_the_fixed_header_set() {
        let (base, rx) = mock_http_server(
            "200 OK",
            "application/json",
            r#"{"content":[{"type":"text","text":"ok"}]}"#.into(),
        )
        .await;
        let _ = controlled_send(
            &messages_spec(base),
            &ControlledRequest::default(),
            "SECRET_CREDENTIAL",
            &ctx_with_tool(),
        )
        .await;
        let (_, headers, _) = parse_request(&rx.await.unwrap());
        for leaked in [
            "openai-organization",
            "openai-project",
            "x-organization",
            "x-leak",
            "traceparent",
        ] {
            assert!(!headers.contains_key(leaked), "leaked header {leaked}");
        }
        for name in headers.keys() {
            assert!(
                ANTHROPIC_ALLOWED.contains(&name.as_str()),
                "unexpected header {name}"
            );
        }
    }

    // AC-04.3: a 503 with no caller retry produces exactly one request.
    #[tokio::test]
    async fn controlled_503_makes_exactly_one_attempt_when_retries_zero() {
        let (base, count) = counting_http_server("503 Service Unavailable", None, "{}").await;
        let resp = controlled_send(
            &messages_spec(base),
            &ControlledRequest::default(),
            "K",
            &ctx_with_tool(),
        )
        .await;
        assert!(resp.error_message.is_some());
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    // req 10: a caller-configured retry count is honored and every attempt is real.
    #[tokio::test]
    async fn controlled_caller_retries_are_visible_and_counted() {
        let (base, count) = counting_http_server("503 Service Unavailable", None, "{}").await;
        let req = ControlledRequest {
            request_retries: 2,
            ..ControlledRequest::default()
        };
        let resp = controlled_send(&messages_spec(base), &req, "K", &ctx_with_tool()).await;
        assert!(resp.error_message.is_some());
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }

    // AC-04.4: a 307 is an endpoint error; it is not followed and no other host
    // is contacted (exactly one request reaches the fake).
    #[tokio::test]
    async fn controlled_307_is_endpoint_error_and_not_followed() {
        let (base, count) = counting_http_server(
            "307 Temporary Redirect",
            Some(("location", "http://127.0.0.1:9/elsewhere")),
            "{}",
        )
        .await;
        let resp = controlled_send(
            &messages_spec(base),
            &ControlledRequest::default(),
            "K",
            &ctx_with_tool(),
        )
        .await;
        let msg = resp.error_message.expect("redirect must be an error");
        assert!(msg.contains("redirect"), "{msg}");
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    // AC-04.1 (Chat body) + AC-04.5: chat sends `"auto"`, no strict, no sampling.
    #[tokio::test]
    async fn controlled_chat_body_and_headers_are_the_fixed_set() {
        let (base, rx) = mock_http_server(
            "200 OK",
            "application/json",
            r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}"#.into(),
        )
        .await;
        let resp = controlled_send(
            &chat_spec(base),
            &ControlledRequest::default(),
            "SECRET_CREDENTIAL",
            &ctx_with_tool(),
        )
        .await;
        assert!(resp.error_message.is_none(), "{:?}", resp.error_message);

        let raw = rx.await.unwrap();
        let (request_line, headers, body) = parse_request(&raw);
        assert!(
            request_line.starts_with("POST /chat/completions "),
            "{request_line}"
        );
        assert_eq!(
            headers.get("authorization").unwrap(),
            "Bearer SECRET_CREDENTIAL"
        );
        assert!(!headers.contains_key("x-api-key"));
        for name in headers.keys() {
            assert!(
                CHAT_ALLOWED.contains(&name.as_str()),
                "unexpected header {name}"
            );
        }

        let obj = serde_json::from_str::<Value>(&body).unwrap();
        let obj = obj.as_object().unwrap();
        let keys: BTreeSet<&str> = obj.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["messages", "model", "tool_choice", "tools"]
                .into_iter()
                .collect::<BTreeSet<&str>>()
        );
        assert_eq!(obj["tool_choice"], Value::from("auto"));
        for banned in ["temperature", "top_p", "top_k", "stream"] {
            assert!(obj.get(banned).is_none(), "sampling field {banned} present");
        }
        // No provider "strict" flag on tool definitions (req 5).
        assert!(obj["tools"][0].get("strict").is_none());
        assert!(obj["tools"][0]["function"].get("strict").is_none());
        assert!(!body.contains("strict"));
    }

    // AC-04.7 (configuration): all five required; partial fails naming the
    // missing ones; wire never inferred; unknown fields refused.
    #[test]
    fn controlled_profile_requires_all_five_values() {
        let partial = serde_json::json!({
            "provider_label": "anthropic",
            "model": "claude-opus-5-5",
            "wire": "messages"
        });
        let err = ControlledProfile::from_json(&partial).unwrap_err();
        assert!(err.contains("missing"), "{err}");
        assert!(err.contains("base_url"), "{err}");
        assert!(err.contains("credential_env"), "{err}");
    }

    #[test]
    fn controlled_profile_rejects_unconfigured_and_invalid_wire() {
        let unknown = serde_json::json!({
            "provider_label": "a", "model": "m", "base_url": "http://x",
            "wire": "messages", "credential_env": "K", "temperature": 0.5
        });
        assert!(ControlledProfile::from_json(&unknown)
            .unwrap_err()
            .contains("unconfigured"));

        let bad_wire = serde_json::json!({
            "provider_label": "a", "model": "m", "base_url": "http://x",
            "wire": "guess", "credential_env": "K"
        });
        assert!(ControlledProfile::from_json(&bad_wire)
            .unwrap_err()
            .contains("wire"));
    }

    #[test]
    fn controlled_profile_accepts_complete_and_parses_request_settings() {
        let full = serde_json::json!({
            "provider_label": "anthropic", "model": "claude-opus-5-5",
            "base_url": "https://api.example.com", "wire": "messages",
            "credential_env": "CTRL_KEY", "max_tokens": 4096, "timeout_ms": 30000,
            "request_retries": 0, "proxy": null,
            "extra_body_fields": {"metadata": {"user_id": "dbunk"}}
        });
        let profile = ControlledProfile::from_json(&full).unwrap();
        assert_eq!(profile.spec.wire, Wire::Messages);
        assert_eq!(profile.request.max_tokens, Some(4096));
        assert_eq!(profile.request.request_retries, 0);
        assert!(profile.request.proxy.is_none());
        assert_eq!(
            profile.request.timeout,
            std::time::Duration::from_millis(30000)
        );
    }

    // R8 (review r3): the caller retry count is bounded by the named
    // model-request ceiling (ADR-025), not a bare literal. The boundary value
    // is accepted; one past it is refused with an error that names the ceiling.
    #[test]
    fn controlled_profile_request_retries_bounded_by_named_ceiling() {
        let at = |n: u64| {
            serde_json::json!({
                "provider_label": "a", "model": "m", "base_url": "http://x",
                "wire": "messages", "credential_env": "K", "max_tokens": 256,
                "request_retries": n
            })
        };
        let ok = ControlledProfile::from_json(&at(CONTROLLED_REQUEST_RETRY_CEILING as u64))
            .expect("the ceiling value itself is accepted");
        assert_eq!(ok.request.request_retries, CONTROLLED_REQUEST_RETRY_CEILING);

        let err = ControlledProfile::from_json(&at(CONTROLLED_REQUEST_RETRY_CEILING as u64 + 1))
            .expect_err("one past the ceiling is refused");
        assert!(
            err.contains(&CONTROLLED_REQUEST_RETRY_CEILING.to_string()),
            "error must name the ceiling value: {err}"
        );
        assert!(
            err.contains("ceiling"),
            "error must name it a ceiling: {err}"
        );
    }

    // req 4/5/6: explicit extras cannot overwrite protected or add forbidden fields.
    #[test]
    fn controlled_extra_body_fields_cannot_break_protections() {
        let mut body = serde_json::Map::new();
        let overwrite = serde_json::json!({"model": "other"})
            .as_object()
            .unwrap()
            .clone();
        assert!(apply_extra_body_fields(&mut body, &overwrite)
            .unwrap_err()
            .contains("protected"));

        let forbidden = serde_json::json!({"temperature": 0.7})
            .as_object()
            .unwrap()
            .clone();
        assert!(apply_extra_body_fields(&mut body, &forbidden)
            .unwrap_err()
            .contains("forbidden"));

        let allowed = serde_json::json!({"metadata": {"k": "v"}})
            .as_object()
            .unwrap()
            .clone();
        apply_extra_body_fields(&mut body, &allowed).unwrap();
        assert_eq!(body["metadata"], serde_json::json!({"k": "v"}));
    }

    // R4 review (R1): extra_body_fields can carry a sampling parameter that the
    // old five-entry denylist never named. FR-04 req 4 bans every sampling
    // parameter and ADR-025 forbids explicit extensions overriding that
    // prohibition. Such a profile must be refused at startup (from_json),
    // before any request is built or sent, naming the offending field.
    #[test]
    fn controlled_profile_rejects_sampling_in_extra_body_fields() {
        let with_extra = |field: &str| {
            serde_json::json!({
                "provider_label": "openrouter", "model": "m",
                "base_url": "http://x", "wire": "chat",
                "credential_env": "K", "extra_body_fields": { field: 1 }
            })
        };
        for field in [
            "frequency_penalty",
            "presence_penalty",
            "logit_bias",
            "logprobs",
            "top_logprobs",
            "n",
            "seed",
            "min_p",
            "repetition_penalty",
            "stream_options",
            "response_format",
            "best_of",
            "parallel_tool_calls",
        ] {
            let err = ControlledProfile::from_json(&with_extra(field))
                .expect_err("a sampling/forced-choice extra must be refused at startup");
            assert!(err.contains(field), "error must name {field}: {err}");
            assert!(
                err.contains("forbidden"),
                "error must call it forbidden: {err}"
            );
        }
    }

    // R4 review (R2): max_tokens comes from the caller (FR-04 req 6). On the
    // Messages wire the field is mandatory, so a Messages profile that omits it
    // must fail at startup naming the field — there is no built-in default on
    // the request path. On the Chat wire the field is optional.
    #[test]
    fn controlled_messages_profile_requires_caller_max_tokens() {
        let p = serde_json::json!({
            "provider_label": "anthropic", "model": "m", "base_url": "http://x",
            "wire": "messages", "credential_env": "K"
        });
        let err = ControlledProfile::from_json(&p)
            .expect_err("a Messages profile without max_tokens must fail at startup");
        assert!(
            err.contains("max_tokens"),
            "error must name max_tokens: {err}"
        );
    }

    #[test]
    fn controlled_chat_profile_allows_omitted_max_tokens() {
        let p = serde_json::json!({
            "provider_label": "openrouter", "model": "m", "base_url": "http://x",
            "wire": "chat", "credential_env": "K"
        });
        assert!(
            ControlledProfile::from_json(&p).is_ok(),
            "a Chat profile may omit max_tokens"
        );
    }

    // R4 review (R2): the Chat wire carries the caller's max_tokens only when
    // the caller set it, and omits the field entirely otherwise — no built-in
    // value is ever injected on the request path.
    #[test]
    fn controlled_chat_body_includes_max_tokens_only_when_caller_set_it() {
        let ctx = ctx_with_tool();
        // Drive through from_json (not a struct literal) so the test is
        // agnostic to the max_tokens field representation.
        let chat_profile = |extra: Value| {
            let mut obj = serde_json::json!({
                "provider_label": "openrouter", "model": "some/model",
                "base_url": "http://x", "wire": "chat", "credential_env": "K"
            });
            obj.as_object_mut().unwrap().extend(
                extra
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone())),
            );
            ControlledProfile::from_json(&obj).unwrap()
        };
        let unset = crate::providers::openrouter::controlled_request(
            &chat_spec("http://x".into()),
            &chat_profile(serde_json::json!({})).request,
            "K",
            &ctx,
        )
        .unwrap();
        assert!(
            unset.body.get("max_tokens").is_none(),
            "Chat omits max_tokens when the caller did not set it"
        );
        let set = crate::providers::openrouter::controlled_request(
            &chat_spec("http://x".into()),
            &chat_profile(serde_json::json!({"max_tokens": 321})).request,
            "K",
            &ctx,
        )
        .unwrap();
        assert_eq!(
            set.body.get("max_tokens"),
            Some(&serde_json::json!(321)),
            "Chat carries the caller's max_tokens when set"
        );
    }

    // H6: a credential value never appears in a Debug representation or in any
    // controlled-mode error message. INTERFACES: "Do not include ... a secret
    // value in error messages." The credential is injected only into wire
    // headers (never logged); failures name status/transport, not the key.
    #[tokio::test]
    async fn controlled_credential_never_appears_in_debug_or_errors() {
        const SECRET: &str = "sk-SUPER-SECRET-TOKEN-7f3a9c1e";
        let _cred = EnvGuard::set("CTRL_H6_KEY", SECRET);
        let profile = ControlledProfile::from_json(&serde_json::json!({
            "provider_label": "anthropic", "model": "claude-opus-5-5",
            "base_url": "http://127.0.0.1:1", "wire": "messages",
            "credential_env": "CTRL_H6_KEY", "max_tokens": 256
        }))
        .unwrap();
        let provider = super::router::ControlledProvider::from_profile(profile).unwrap();
        let debug = format!("{provider:?}");
        assert!(
            !debug.contains(SECRET),
            "credential leaked in Debug: {debug}"
        );
        assert!(debug.contains("[redacted]"), "debug must redact: {debug}");

        // A non-2xx endpoint failure never names the credential.
        let (base, _count) = counting_http_server("503 Service Unavailable", None, "{}").await;
        let resp = controlled_send(
            &messages_spec(base),
            &ControlledRequest::default(),
            SECRET,
            &ctx_with_tool(),
        )
        .await;
        let msg = resp.error_message.expect("503 must be an error");
        assert!(!msg.contains(SECRET), "credential leaked in error: {msg}");

        // A redirect endpoint error likewise names no credential.
        let (base2, _c2) = counting_http_server(
            "307 Temporary Redirect",
            Some(("location", "http://127.0.0.1:9/elsewhere")),
            "{}",
        )
        .await;
        let resp2 = controlled_send(
            &messages_spec(base2),
            &ControlledRequest::default(),
            SECRET,
            &ctx_with_tool(),
        )
        .await;
        assert!(!resp2
            .error_message
            .expect("redirect must be an error")
            .contains(SECRET));
    }

    // R4 (review r3) / req 11: a controlled request with a short `timeout_ms`
    // terminates with a timeout error well inside the bound, never waiting for
    // a slow endpoint to reply.
    #[tokio::test]
    async fn controlled_request_times_out_against_a_slow_endpoint() {
        let base = slow_http_server(5_000).await;
        let req = ControlledRequest {
            timeout: std::time::Duration::from_millis(150),
            ..ControlledRequest::default()
        };
        let start = std::time::Instant::now();
        let resp = controlled_send(&messages_spec(base), &req, "K", &ctx_with_tool()).await;
        let elapsed = start.elapsed();
        let msg = resp.error_message.expect("a timed-out request must error");
        assert!(
            msg.to_lowercase().contains("time"),
            "error must indicate a timeout: {msg}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "must end at the 150ms caller timeout, not wait for the 5s endpoint (took {elapsed:?})"
        );
    }

    // R5 (review r3) / req 13: an explicit `proxy` is honored — the request
    // goes to the proxy, and the endpoint host is NOT contacted directly.
    // (Ambient HTTP(S)_PROXY/ALL_PROXY being IGNORED is proven at the process
    // level in tests/test_request_control.py with a counting proxy server.)
    #[tokio::test]
    async fn controlled_explicit_proxy_is_used_and_endpoint_not_contacted_directly() {
        let (endpoint, endpoint_hits) = counting_http_server("200 OK", None, "{}").await;
        let (proxy, proxy_hits) = counting_http_server("503 Service Unavailable", None, "{}").await;
        let req = ControlledRequest {
            proxy: Some(proxy.clone()),
            ..ControlledRequest::default()
        };
        let _ = controlled_send(&messages_spec(endpoint), &req, "K", &ctx_with_tool()).await;
        assert_eq!(
            proxy_hits.load(Ordering::SeqCst),
            1,
            "the explicit proxy must receive the request"
        );
        assert_eq!(
            endpoint_hits.load(Ordering::SeqCst),
            0,
            "the endpoint host must not be contacted directly when a proxy is set"
        );
    }

    // R7 (review r3) / AC-04.5: a fake Messages endpoint that returns HTTP 400
    // for any FORCED tool_choice ACCEPTS the controlled request, because the
    // controlled wire always sends {"type":"auto"}. The 400 path is never hit.
    #[tokio::test]
    async fn controlled_messages_never_provokes_the_forced_choice_rejection() {
        let (base, rejected) = forced_choice_rejecting_messages_server().await;
        let resp = controlled_send(
            &messages_spec(base),
            &ControlledRequest::default(),
            "K",
            &ctx_with_tool(),
        )
        .await;
        assert!(
            resp.error_message.is_none(),
            "the auto tool_choice must be accepted (not a forced-choice 400): {:?}",
            resp.error_message
        );
        assert_eq!(
            rejected.load(Ordering::SeqCst),
            0,
            "the endpoint must never reject the controlled request as a forced choice"
        );
    }

    // R3 (review r3) / AC-04.2: with a tracing provider installed and an active
    // trace span, NO trace-propagation or telemetry header travels on EITHER
    // wire, and the header set stays the fixed allowed set. The controlled
    // client has no propagation middleware; this guards a future addition.
    #[tokio::test]
    async fn controlled_headers_fixed_under_active_tracing_both_wires() {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(std::io::sink)
            .finish();
        let _provider = tracing::subscriber::set_default(subscriber);
        let span = tracing::info_span!("controlled-request", trace_id = "deadbeefcafef00d");
        tracing::info!(parent: &span, "controlled request under an active tracing provider");

        const TRACE_HEADERS: [&str; 6] = [
            "traceparent",
            "tracestate",
            "baggage",
            "b3",
            "x-b3-traceid",
            "x-b3-spanid",
        ];

        // Messages wire.
        let (base, rx) = mock_http_server(
            "200 OK",
            "application/json",
            r#"{"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}"#.into(),
        )
        .await;
        let _ = controlled_send(
            &messages_spec(base),
            &ControlledRequest::default(),
            "K",
            &ctx_with_tool(),
        )
        .await;
        let (_, headers, _) = parse_request(&rx.await.unwrap());
        for h in TRACE_HEADERS {
            assert!(
                !headers.contains_key(h),
                "trace header {h} leaked on messages wire"
            );
        }
        for name in headers.keys() {
            assert!(
                ANTHROPIC_ALLOWED.contains(&name.as_str()),
                "unexpected header {name} on messages wire"
            );
        }

        // Chat wire.
        let (base, rx) = mock_http_server(
            "200 OK",
            "application/json",
            r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}"#.into(),
        )
        .await;
        let _ = controlled_send(
            &chat_spec(base),
            &ControlledRequest::default(),
            "K",
            &ctx_with_tool(),
        )
        .await;
        let (_, headers, _) = parse_request(&rx.await.unwrap());
        for h in TRACE_HEADERS {
            assert!(
                !headers.contains_key(h),
                "trace header {h} leaked on chat wire"
            );
        }
        for name in headers.keys() {
            assert!(
                CHAT_ALLOWED.contains(&name.as_str()),
                "unexpected header {name} on chat wire"
            );
        }
    }
}
