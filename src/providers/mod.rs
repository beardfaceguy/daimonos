#![allow(dead_code)]

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod anthropic;
pub mod openrouter;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentBlock {
    Text(String),
    Thinking(String),
    ToolCall { id: String, name: String, input: Value },
    ToolResult { tool_use_id: String, content: String, is_error: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Message { role: Role::User, content: vec![ContentBlock::Text(text.into())] }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Message { role: Role::Assistant, content: vec![ContentBlock::Text(text.into())] }
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
    Aborted,
    MaxTokens,
}

impl StopReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            StopReason::EndTurn => "end_turn",
            StopReason::ToolUse => "tool_use",
            StopReason::Error => "error",
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
    pub usage: Usage,
}

impl LlmResponse {
    pub fn error(msg: impl Into<String>) -> Self {
        LlmResponse {
            content: vec![],
            stop_reason: StopReason::Error,
            error_message: Some(msg.into()),
            context_overflow: false,
            usage: Usage::default(),
        }
    }

    /// An error response classified as a context-window overflow.
    pub fn context_overflow_error(msg: impl Into<String>) -> Self {
        LlmResponse { context_overflow: true, ..Self::error(msg) }
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

pub struct CompleteOpts {
    pub model: String,
    pub max_tokens: u32,
    pub thinking: ThinkingLevel,
}

impl Default for CompleteOpts {
    fn default() -> Self {
        CompleteOpts {
            model: "claude-opus-4-8".to_string(),
            max_tokens: 8192,
            thinking: ThinkingLevel::default(),
        }
    }
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

    /// Like `complete`, but invokes `on_event` with each `StreamEvent` as it
    /// arrives, before returning the same final `LlmResponse` `complete`
    /// would produce. Default: no incremental events, just delegates to
    /// `complete` — so providers (and test doubles) that don't override this
    /// keep working unchanged.
    async fn stream(
        &self,
        ctx: &Context,
        opts: &CompleteOpts,
        _on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> LlmResponse {
        self.complete(ctx, opts).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StaticProvider(LlmResponse);

    #[async_trait]
    impl LlmProvider for StaticProvider {
        async fn complete(&self, _ctx: &Context, _opts: &CompleteOpts) -> LlmResponse {
            self.0.clone()
        }
    }

    fn dummy_ctx() -> Context {
        Context { messages: vec![], system: None, tools: vec![], stable_prefix_len: 0 }
    }

    #[tokio::test]
    async fn default_stream_delegates_to_complete() {
        let provider = StaticProvider(LlmResponse {
            content: vec![ContentBlock::Text("hi".into())],
            stop_reason: StopReason::EndTurn,
            error_message: None,
            context_overflow: false,
            usage: Usage::default(),
        });
        let mut events = Vec::new();
        let resp = provider
            .stream(&dummy_ctx(), &CompleteOpts::default(), &mut |e| events.push(e))
            .await;
        assert!(matches!(&resp.content[0], ContentBlock::Text(t) if t == "hi"));
        assert!(events.is_empty(), "default stream() must not synthesize events");
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
        assert_eq!(u.cache_read, 0);
        assert_eq!(u.cache_write, 0);
        assert_eq!(u.cost.total_usd, 0.0);
    }

    #[test]
    fn usage_prompt_tokens_sums_all_window_occupants() {
        // Canonical semantics: input is non-cached; cached tokens still
        // occupy the window, so prompt_tokens() counts all three.
        let u = Usage { input: 100, output: 9, cache_read: 30, cache_write: 20, cost: Cost::default() };
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
    fn message_user_helper() {
        let m = Message::user("hello");
        assert_eq!(m.role, Role::User);
        assert!(matches!(&m.content[0], ContentBlock::Text(t) if t == "hello"));
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
        assert_eq!(StopReason::Aborted.as_str(), "aborted");
        assert_eq!(StopReason::MaxTokens.as_str(), "max_tokens");
    }

    #[test]
    fn stop_reason_variants_are_distinct() {
        let variants = [
            StopReason::EndTurn,
            StopReason::ToolUse,
            StopReason::Error,
            StopReason::Aborted,
            StopReason::MaxTokens,
        ];
        for (i, a) in variants.iter().enumerate() {
            for (j, b) in variants.iter().enumerate() {
                if i == j { assert_eq!(a, b); } else { assert_ne!(a, b); }
            }
        }
    }
}
