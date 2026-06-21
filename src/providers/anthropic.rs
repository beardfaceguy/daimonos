#![allow(dead_code)]

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    CompleteOpts, ContentBlock, Context, Cost, LlmProvider, LlmResponse, Message, Role,
    StopReason, ThinkingLevel, Usage,
};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

// --- Anthropic wire types (request) ---

#[derive(Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool>,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinking>,
}

#[derive(Serialize)]
struct AnthropicThinking {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: Value,
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    content: Vec<AnthropicBlock>,
}

#[derive(Serialize)]
struct CacheControl {
    #[serde(rename = "type")]
    kind: String,
}

impl CacheControl {
    fn ephemeral() -> Self {
        CacheControl { kind: "ephemeral".to_string() }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}

// --- Anthropic wire types (response) ---

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicResponseBlock>,
    stop_reason: Option<String>,
    #[serde(default)]
    usage: AnthropicUsage,
}

/// Flat struct for response blocks — avoids serde tag+other limitations.
#[derive(Deserialize)]
struct AnthropicResponseBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<Value>,
}

impl AnthropicResponseBlock {
    fn into_content(self) -> Option<ContentBlock> {
        match self.kind.as_str() {
            "text" => self.text.map(ContentBlock::Text),
            "thinking" => self.thinking.map(ContentBlock::Thinking),
            "tool_use" => {
                if let (Some(id), Some(name), Some(input)) = (self.id, self.name, self.input) {
                    Some(ContentBlock::ToolCall { id, name, input })
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

#[derive(Deserialize, Default)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

// --- Pricing ---

struct Pricing {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
}

fn pricing_for(model: &str) -> Pricing {
    if model.starts_with("claude-haiku-4") {
        Pricing { input: 1.0, output: 5.0, cache_read: 0.1, cache_write: 1.25 }
    } else if model.starts_with("claude-sonnet-4") {
        Pricing { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75 }
    } else {
        // Opus 4.x and Fable 5
        Pricing { input: 5.0, output: 25.0, cache_read: 0.5, cache_write: 6.25 }
    }
}

// --- Pure conversion helpers ---

fn map_usage(raw: AnthropicUsage, model: &str) -> Usage {
    let p = pricing_for(model);
    let input_usd = raw.input_tokens as f64 / 1_000_000.0 * p.input;
    let output_usd = raw.output_tokens as f64 / 1_000_000.0 * p.output;
    let cache_read_usd = raw.cache_read_input_tokens as f64 / 1_000_000.0 * p.cache_read;
    let cache_write_usd = raw.cache_creation_input_tokens as f64 / 1_000_000.0 * p.cache_write;
    Usage {
        input: raw.input_tokens,
        output: raw.output_tokens,
        cache_read: raw.cache_read_input_tokens,
        cache_write: raw.cache_creation_input_tokens,
        cost: Cost {
            input_usd,
            output_usd,
            cache_read_usd,
            cache_write_usd,
            total_usd: input_usd + output_usd + cache_read_usd + cache_write_usd,
        },
    }
}

pub(crate) fn map_stop_reason(s: Option<&str>) -> StopReason {
    match s {
        Some("end_turn") | Some("stop_sequence") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        _ => StopReason::Error,
    }
}

fn supports_adaptive_thinking(model: &str) -> bool {
    model.starts_with("claude-opus-4")
        || model.starts_with("claude-sonnet-4-6")
        || model.starts_with("claude-fable")
}

fn content_block_to_anthropic(block: &ContentBlock, cache: Option<CacheControl>) -> AnthropicBlock {
    match block {
        ContentBlock::Text(t) => AnthropicBlock::Text { text: t.clone(), cache_control: cache },
        ContentBlock::Thinking(t) => AnthropicBlock::Thinking { thinking: t.clone(), cache_control: cache },
        ContentBlock::ToolCall { id, name, input } => AnthropicBlock::ToolUse {
            id: id.clone(), name: name.clone(), input: input.clone(), cache_control: cache,
        },
        ContentBlock::ToolResult { tool_use_id, content, is_error } => AnthropicBlock::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: content.clone(),
            is_error: if *is_error { Some(true) } else { None },
            cache_control: cache,
        },
    }
}

fn message_to_anthropic(msg: &Message, is_prefix_boundary: bool) -> AnthropicMessage {
    let last = msg.content.len().saturating_sub(1);
    let blocks = msg.content.iter().enumerate().map(|(i, block)| {
        let cache = if is_prefix_boundary && i == last {
            Some(CacheControl::ephemeral())
        } else {
            None
        };
        content_block_to_anthropic(block, cache)
    }).collect();
    AnthropicMessage {
        role: match msg.role { Role::User => "user".to_string(), Role::Assistant => "assistant".to_string() },
        content: blocks,
    }
}

fn build_request(ctx: &Context, opts: &CompleteOpts) -> AnthropicRequest {
    let tools = ctx.tools.iter().map(|t| AnthropicTool {
        name: t.name.clone(),
        description: t.description.clone(),
        input_schema: t.input_schema.clone(),
    }).collect();

    let messages = ctx.messages.iter().enumerate().map(|(i, msg)| {
        // Mark the last message of the stable prefix as the cache boundary
        let is_boundary = ctx.stable_prefix_len > 0 && i + 1 == ctx.stable_prefix_len;
        message_to_anthropic(msg, is_boundary)
    }).collect();

    let thinking = if opts.thinking != ThinkingLevel::Off && supports_adaptive_thinking(&opts.model) {
        Some(AnthropicThinking { kind: "adaptive".to_string() })
    } else {
        None
    };

    AnthropicRequest {
        model: opts.model.clone(),
        max_tokens: opts.max_tokens,
        system: ctx.system.clone(),
        tools,
        messages,
        thinking,
    }
}

// --- Provider ---

pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("base_url", &self.base_url)
            .field("api_key", &"[redacted]")
            .finish()
    }
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        AnthropicProvider {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn from_env() -> Result<Self, String> {
        let key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| "ANTHROPIC_API_KEY not set".to_string())?;
        Ok(AnthropicProvider::new(key))
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn complete(&self, ctx: &Context, opts: &CompleteOpts) -> LlmResponse {
        let request = build_request(ctx, opts);

        let result = self.client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await;

        let resp = match result {
            Err(e) => return LlmResponse::error(format!("network error: {e}")),
            Ok(r) => r,
        };

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return LlmResponse::error(format!("API {status}: {body}"));
        }

        let raw: AnthropicResponse = match resp.json().await {
            Err(e) => return LlmResponse::error(format!("parse error: {e}")),
            Ok(r) => r,
        };

        let content = raw.content.into_iter().filter_map(|b| b.into_content()).collect();
        let stop_reason = map_stop_reason(raw.stop_reason.as_deref());
        let usage = map_usage(raw.usage, &opts.model);

        LlmResponse { content, stop_reason, error_message: None, usage }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ToolSchema;
    use serde_json::json;

    // --- map_stop_reason ---

    #[test]
    fn stop_reason_end_turn() {
        assert_eq!(map_stop_reason(Some("end_turn")), StopReason::EndTurn);
    }

    #[test]
    fn stop_reason_stop_sequence_maps_to_end_turn() {
        assert_eq!(map_stop_reason(Some("stop_sequence")), StopReason::EndTurn);
    }

    #[test]
    fn stop_reason_tool_use() {
        assert_eq!(map_stop_reason(Some("tool_use")), StopReason::ToolUse);
    }

    #[test]
    fn stop_reason_max_tokens() {
        assert_eq!(map_stop_reason(Some("max_tokens")), StopReason::MaxTokens);
    }

    #[test]
    fn stop_reason_unknown_maps_to_error() {
        assert_eq!(map_stop_reason(Some("unknown_future")), StopReason::Error);
        assert_eq!(map_stop_reason(None), StopReason::Error);
    }

    // --- map_usage ---

    #[test]
    fn map_usage_tokens_pass_through() {
        let raw = AnthropicUsage {
            input_tokens: 1_000,
            output_tokens: 500,
            cache_read_input_tokens: 2_000,
            cache_creation_input_tokens: 100,
        };
        let u = map_usage(raw, "claude-opus-4-8");
        assert_eq!(u.input, 1_000);
        assert_eq!(u.output, 500);
        assert_eq!(u.cache_read, 2_000);
        assert_eq!(u.cache_write, 100);
    }

    #[test]
    fn map_usage_opus_cost_computation() {
        // Opus 4.8: input $5/M, output $25/M, cache_read $0.5/M, cache_write $6.25/M
        let raw = AnthropicUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_input_tokens: 1_000_000,
            cache_creation_input_tokens: 1_000_000,
        };
        let u = map_usage(raw, "claude-opus-4-8");
        assert!((u.cost.input_usd - 5.0).abs() < 1e-6);
        assert!((u.cost.output_usd - 25.0).abs() < 1e-6);
        assert!((u.cost.cache_read_usd - 0.5).abs() < 1e-6);
        assert!((u.cost.cache_write_usd - 6.25).abs() < 1e-6);
        assert!((u.cost.total_usd - 36.75).abs() < 1e-6);
    }

    #[test]
    fn map_usage_sonnet_cost_computation() {
        // Sonnet 4.6: input $3/M, output $15/M
        let raw = AnthropicUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        };
        let u = map_usage(raw, "claude-sonnet-4-6");
        assert!((u.cost.input_usd - 3.0).abs() < 1e-6);
        assert!((u.cost.output_usd - 15.0).abs() < 1e-6);
    }

    #[test]
    fn map_usage_haiku_cost_computation() {
        let raw = AnthropicUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        };
        let u = map_usage(raw, "claude-haiku-4-5");
        assert!((u.cost.input_usd - 1.0).abs() < 1e-6);
        assert!((u.cost.output_usd - 5.0).abs() < 1e-6);
    }

    #[test]
    fn map_usage_zero_tokens_zero_cost() {
        let raw = AnthropicUsage::default();
        let u = map_usage(raw, "claude-opus-4-8");
        assert_eq!(u.input, 0);
        assert_eq!(u.cost.total_usd, 0.0);
    }

    // --- build_request ---

    #[test]
    fn build_request_no_sampling_params() {
        let ctx = Context {
            messages: vec![Message::user("hello")],
            system: None,
            tools: vec![],
            stable_prefix_len: 0,
        };
        let json = serde_json::to_value(build_request(&ctx, &CompleteOpts::default())).unwrap();
        assert!(json.get("temperature").is_none(), "temperature must not appear");
        assert!(json.get("top_p").is_none(), "top_p must not appear");
        assert!(json.get("top_k").is_none(), "top_k must not appear");
        assert!(json.get("budget_tokens").is_none(), "budget_tokens must not appear");
    }

    #[test]
    fn build_request_includes_adaptive_thinking_for_opus() {
        let ctx = Context {
            messages: vec![Message::user("hello")],
            system: None, tools: vec![], stable_prefix_len: 0,
        };
        let opts = CompleteOpts { model: "claude-opus-4-8".to_string(), ..CompleteOpts::default() };
        let json = serde_json::to_value(build_request(&ctx, &opts)).unwrap();
        assert_eq!(json["thinking"]["type"], "adaptive");
    }

    #[test]
    fn build_request_no_thinking_for_haiku() {
        let ctx = Context {
            messages: vec![Message::user("hello")],
            system: None, tools: vec![], stable_prefix_len: 0,
        };
        let opts = CompleteOpts { model: "claude-haiku-4-5".to_string(), ..CompleteOpts::default() };
        let json = serde_json::to_value(build_request(&ctx, &opts)).unwrap();
        assert!(json.get("thinking").is_none() || json["thinking"].is_null());
    }

    #[test]
    fn build_request_thinking_off_suppresses_thinking() {
        let ctx = Context {
            messages: vec![Message::user("hello")],
            system: None, tools: vec![], stable_prefix_len: 0,
        };
        let opts = CompleteOpts {
            model: "claude-opus-4-8".to_string(),
            thinking: ThinkingLevel::Off,
            ..CompleteOpts::default()
        };
        let json = serde_json::to_value(build_request(&ctx, &opts)).unwrap();
        assert!(json.get("thinking").is_none() || json["thinking"].is_null());
    }

    #[test]
    fn build_request_cache_control_on_prefix_boundary() {
        let ctx = Context {
            messages: vec![Message::user("system context"), Message::user("user task")],
            system: None,
            tools: vec![],
            stable_prefix_len: 1,  // first message is stable
        };
        let json = serde_json::to_value(build_request(&ctx, &CompleteOpts::default())).unwrap();
        let first_block = &json["messages"][0]["content"][0];
        assert_eq!(first_block["cache_control"]["type"], "ephemeral");
        // Second message (volatile) must not have cache_control
        let second_block = &json["messages"][1]["content"][0];
        assert!(second_block.get("cache_control").is_none() || second_block["cache_control"].is_null());
    }

    #[test]
    fn build_request_no_cache_control_when_stable_prefix_len_zero() {
        let ctx = Context {
            messages: vec![Message::user("hello")],
            system: None, tools: vec![], stable_prefix_len: 0,
        };
        let json = serde_json::to_value(build_request(&ctx, &CompleteOpts::default())).unwrap();
        let block = &json["messages"][0]["content"][0];
        assert!(block.get("cache_control").is_none() || block["cache_control"].is_null());
    }

    #[test]
    fn build_request_tools_serialized() {
        let ctx = Context {
            messages: vec![Message::user("go")],
            system: None,
            tools: vec![ToolSchema {
                name: "read_file".to_string(),
                description: "Read a file".to_string(),
                input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            }],
            stable_prefix_len: 0,
        };
        let json = serde_json::to_value(build_request(&ctx, &CompleteOpts::default())).unwrap();
        assert_eq!(json["tools"][0]["name"], "read_file");
        assert_eq!(json["tools"][0]["description"], "Read a file");
    }

    // --- from_env ---

    #[test]
    fn from_env_errors_when_key_not_set() {
        // Unset the env var in a temp scope — use a unique name to avoid test interference
        std::env::remove_var("ANTHROPIC_API_KEY");
        let result = AnthropicProvider::from_env();
        // Only assert error if key is genuinely absent (CI may have it set)
        if std::env::var("ANTHROPIC_API_KEY").is_err() {
            assert!(result.is_err());
            assert!(result.unwrap_err().contains("ANTHROPIC_API_KEY"));
        }
    }

    // --- complete() error paths ---

    #[tokio::test]
    async fn complete_returns_error_on_api_401() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        // Start a local server that returns 401 then close
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let resp = b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 27\r\nconnection: close\r\n\r\n{\"error\":{\"type\":\"auth_error\"}}";
                stream.write_all(resp).await.ok();
            }
        });

        let provider = AnthropicProvider::new("bad-key")
            .with_base_url(format!("http://127.0.0.1:{port}"));
        let ctx = Context {
            messages: vec![Message::user("hello")],
            system: None, tools: vec![], stable_prefix_len: 0,
        };
        let resp = provider.complete(&ctx, &CompleteOpts::default()).await;
        assert_eq!(resp.stop_reason, StopReason::Error);
        assert!(resp.error_message.is_some());
        assert!(resp.content.is_empty());
    }

    // --- response block parsing ---

    #[test]
    fn response_block_text_parses() {
        let block: AnthropicResponseBlock = serde_json::from_value(json!({
            "type": "text",
            "text": "hello world"
        })).unwrap();
        assert!(matches!(block.into_content(), Some(ContentBlock::Text(t)) if t == "hello world"));
    }

    #[test]
    fn response_block_tool_use_parses() {
        let block: AnthropicResponseBlock = serde_json::from_value(json!({
            "type": "tool_use",
            "id": "toolu_01",
            "name": "read_file",
            "input": {"path": "foo.txt"}
        })).unwrap();
        assert!(matches!(block.into_content(), Some(ContentBlock::ToolCall { name, .. }) if name == "read_file"));
    }

    #[test]
    fn response_block_unknown_type_returns_none() {
        let block: AnthropicResponseBlock = serde_json::from_value(json!({
            "type": "redacted_thinking",
            "data": "opaque"
        })).unwrap();
        assert!(block.into_content().is_none());
    }
}
