#![allow(dead_code)]

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod anthropic;
pub mod openai;
pub mod openrouter;

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
}

#[cfg(test)]
mod tests {
    use super::*;

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
