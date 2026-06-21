#![allow(dead_code)]

use async_trait::async_trait;
use serde_json::Value;

pub mod anthropic;
pub mod openrouter;

#[derive(Debug, Clone, PartialEq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone)]
pub enum ContentBlock {
    Text(String),
    Thinking(String),
    ToolCall { id: String, name: String, input: Value },
    ToolResult { tool_use_id: String, content: String, is_error: bool },
}

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: Cost,
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
    pub usage: Usage,
}

impl LlmResponse {
    pub fn error(msg: impl Into<String>) -> Self {
        LlmResponse {
            content: vec![],
            stop_reason: StopReason::Error,
            error_message: Some(msg.into()),
            usage: Usage::default(),
        }
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

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn complete(&self, ctx: &Context, opts: &CompleteOpts) -> LlmResponse;
}

#[cfg(test)]
mod tests {
    use super::*;

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
