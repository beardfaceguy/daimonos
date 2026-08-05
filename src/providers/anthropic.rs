#![allow(dead_code)]

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{
    CompleteOpts, ContentBlock, Context, Cost, LlmProvider, LlmResponse, Message, Role, StopReason,
    StreamEvent, ThinkingLevel, Usage,
};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const PROVIDER_STATE: &str = "anthropic";
const THINKING_SIGNATURE_STATE: &str = "thinking_signature";

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
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    stream: bool,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    content: Vec<AnthropicBlock>,
}

#[derive(Serialize)]
struct AnthropicImageSource {
    #[serde(rename = "type")]
    kind: String,
    media_type: String,
    data: String,
}

#[derive(Serialize)]
struct CacheControl {
    #[serde(rename = "type")]
    kind: String,
}

impl CacheControl {
    fn ephemeral() -> Self {
        CacheControl {
            kind: "ephemeral".to_string(),
        }
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
    Image {
        source: AnthropicImageSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
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
    signature: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<Value>,
}

impl AnthropicResponseBlock {
    fn into_contents(self) -> Vec<ContentBlock> {
        match self.kind.as_str() {
            "text" => self.text.map(ContentBlock::Text).into_iter().collect(),
            "thinking" => {
                let Some(thinking) = self.thinking else {
                    return Vec::new();
                };
                let mut content = vec![ContentBlock::Thinking(thinking)];
                if let Some(signature) = self.signature.filter(|value| !value.is_empty()) {
                    content.push(ContentBlock::ProviderState {
                        provider: PROVIDER_STATE.to_string(),
                        data: json!({
                            "type": THINKING_SIGNATURE_STATE,
                            "signature": signature,
                        }),
                    });
                }
                content
            }
            "tool_use" => {
                if let (Some(id), Some(name), Some(input)) = (self.id, self.name, self.input) {
                    vec![ContentBlock::ToolCall { id, name, input }]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
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
        Pricing {
            input: 1.0,
            output: 5.0,
            cache_read: 0.1,
            cache_write: 1.25,
        }
    } else if model.starts_with("claude-sonnet-4") {
        Pricing {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
        }
    } else {
        // Opus 4.x and Fable 5
        Pricing {
            input: 5.0,
            output: 25.0,
            cache_read: 0.5,
            cache_write: 6.25,
        }
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
        reasoning_output: 0,
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
        Some("refusal") => StopReason::Refusal,
        _ => StopReason::Error,
    }
}

/// Classify an Anthropic error body/message as a context-window overflow
/// (ADR-002 reactive compaction). Provider-local knowledge, per ADR-001.
/// Anthropic's two phrasings: "prompt is too long: N tokens > M maximum"
/// and "input length and `max_tokens` exceed context limit".
pub(crate) fn is_context_overflow_error(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("prompt is too long") || m.contains("exceed context limit")
}

/// Extract the context window from an Anthropic `GET /v1/models/{id}` body
/// (vikunja #965). Anthropic's Models API exposes it as `max_input_tokens`.
/// `None` when the field is absent or zero.
pub(crate) fn max_input_tokens_from_model(body: &Value) -> Option<u64> {
    body["max_input_tokens"].as_u64().filter(|&n| n > 0)
}

/// The largest context window Anthropic actually serves this adapter.
///
/// The models endpoint's `max_input_tokens` reports the absolute API input
/// ceiling — the 1M long-context beta — which is only reachable when a
/// request carries the `context-1m` beta header. This adapter never sends
/// that header, so requests are rejected at the standard ~200k window
/// regardless of what the endpoint advertises. Treating the 1M ceiling as
/// the window made every downstream consumer compute against ~5x reality:
/// ADR-002 compaction thresholds landed near 744k tokens and never fired
/// while sessions exhausted the real window, and context-usage projections
/// showed mostly-free context on nearly-full sessions (vikunja #1228).
///
/// An explicit `DAIMONOS_AGENT_CONTEXT_WINDOW` in the agent env still
/// overrides provider resolution entirely for users who do have 1M access;
/// revisit this cap if the adapter ever sends the `context-1m` beta header.
const SERVING_WINDOW_CAP: u64 = 200_000;

/// Clamp a models-endpoint `max_input_tokens` to the window this adapter's
/// requests can actually use.
pub(crate) fn effective_context_window(reported: u64) -> u64 {
    reported.min(SERVING_WINDOW_CAP)
}

fn supports_adaptive_thinking(model: &str) -> bool {
    model.starts_with("claude-opus-4")
        || model.starts_with("claude-opus-5")
        || model.starts_with("claude-sonnet-4-6")
        || model.starts_with("claude-fable")
}

fn content_block_to_anthropic(
    block: &ContentBlock,
    next_block: Option<&ContentBlock>,
    cache: Option<CacheControl>,
) -> Option<AnthropicBlock> {
    Some(match block {
        ContentBlock::Text(t) => AnthropicBlock::Text {
            text: t.clone(),
            cache_control: cache,
        },
        ContentBlock::Image {
            data, media_type, ..
        } => AnthropicBlock::Image {
            source: AnthropicImageSource {
                kind: "base64".to_string(),
                media_type: media_type.clone(),
                data: data.clone(),
            },
            cache_control: cache,
        },
        ContentBlock::Thinking(t) => AnthropicBlock::Thinking {
            thinking: t.clone(),
            signature: thinking_signature(next_block).map(str::to_owned),
            cache_control: cache,
        },
        ContentBlock::ToolCall { id, name, input } => AnthropicBlock::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
            cache_control: cache,
        },
        ContentBlock::ProviderState { .. } => return None,
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => AnthropicBlock::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: content.clone(),
            is_error: if *is_error { Some(true) } else { None },
            cache_control: cache,
        },
    })
}

fn thinking_signature(block: Option<&ContentBlock>) -> Option<&str> {
    let ContentBlock::ProviderState { provider, data } = block? else {
        return None;
    };
    if provider != PROVIDER_STATE || data["type"].as_str() != Some(THINKING_SIGNATURE_STATE) {
        return None;
    }
    data["signature"]
        .as_str()
        .filter(|signature| !signature.is_empty())
}

fn message_to_anthropic(msg: &Message, is_prefix_boundary: bool) -> Option<AnthropicMessage> {
    let last = msg
        .content
        .iter()
        .rposition(|block| !matches!(block, ContentBlock::ProviderState { .. }));
    let blocks: Vec<AnthropicBlock> = msg
        .content
        .iter()
        .enumerate()
        .filter_map(|(i, block)| {
            let cache = if is_prefix_boundary && Some(i) == last {
                Some(CacheControl::ephemeral())
            } else {
                None
            };
            content_block_to_anthropic(block, msg.content.get(i + 1), cache)
        })
        .collect();
    (!blocks.is_empty()).then(|| AnthropicMessage {
        role: match msg.role {
            Role::User => "user".to_string(),
            Role::Assistant => "assistant".to_string(),
        },
        content: blocks,
    })
}

fn build_request_with_cache(
    ctx: &Context,
    opts: &CompleteOpts,
    prompt_cache: bool,
) -> AnthropicRequest {
    let last_tool = ctx.tools.len().checked_sub(1);
    let tools = ctx
        .tools
        .iter()
        .enumerate()
        .map(|(index, t)| AnthropicTool {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: t.input_schema.clone(),
            cache_control: (prompt_cache && Some(index) == last_tool).then(CacheControl::ephemeral),
        })
        .collect();

    let stable_end = ctx.stable_prefix_len.min(ctx.messages.len());
    let boundary_index = ctx.messages[..stable_end].iter().rposition(|message| {
        message
            .content
            .iter()
            .any(|block| !matches!(block, ContentBlock::ProviderState { .. }))
    });
    let messages = ctx
        .messages
        .iter()
        .enumerate()
        .filter_map(|(i, msg)| message_to_anthropic(msg, boundary_index == Some(i)))
        .collect();

    let thinking_kind = if opts.thinking == ThinkingLevel::Off {
        // Opus 5 enables adaptive thinking when the field is omitted, so an
        // explicit disabled value is required to honor the caller's intent.
        opts.model
            .starts_with("claude-opus-5")
            .then_some("disabled")
    } else {
        supports_adaptive_thinking(&opts.model).then_some("adaptive")
    };
    let thinking = thinking_kind.map(|kind| AnthropicThinking {
        kind: kind.to_string(),
    });

    // Anthropic rejects a custom temperature when thinking is enabled —
    // drop it in that combination (core expresses intent, the provider
    // translates; ADR-001). The only setter today is the compaction
    // summarizer, which runs with thinking Off.
    let temperature = if thinking.is_some() {
        None
    } else {
        opts.temperature
    };

    AnthropicRequest {
        model: opts.model.clone(),
        max_tokens: opts.max_tokens,
        system: ctx.system.clone(),
        tools,
        messages,
        thinking,
        temperature,
        stream: false,
    }
}

#[cfg(test)]
fn build_request(ctx: &Context, opts: &CompleteOpts) -> AnthropicRequest {
    build_request_with_cache(ctx, opts, false)
}

// --- Streaming (vikunja #957) ---

/// One in-progress content block, tracked by SSE `index` from
/// `content_block_start` through `content_block_delta`/`_stop`.
enum PartialBlock {
    Text(String),
    Thinking {
        text: String,
        signature: String,
    },
    ToolUse {
        id: String,
        name: String,
        partial_json: String,
    },
}

/// Accumulates Anthropic's `message_start`/`content_block_*`/`message_delta`
/// SSE event stream into the same `LlmResponse` shape `complete` builds from
/// one JSON body, while surfacing text/thinking deltas live via `on_data`.
#[derive(Default)]
struct StreamState {
    blocks: Vec<PartialBlock>,
    stop_reason: Option<String>,
    usage: AnthropicUsage,
}

impl Default for PartialBlock {
    fn default() -> Self {
        PartialBlock::Text(String::new())
    }
}

impl StreamState {
    /// Feed one decoded SSE `data:` JSON payload. Returns text/thinking
    /// deltas to forward live; tool-call inputs accumulate silently and only
    /// appear in the final `finish()` response.
    fn on_data(&mut self, data: &Value) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        match data["type"].as_str() {
            Some("message_start") => {
                if let Ok(u) = serde_json::from_value(data["message"]["usage"].clone()) {
                    self.usage = u;
                }
            }
            Some("content_block_start") => {
                let idx = data["index"].as_u64().unwrap_or(0) as usize;
                let cb = &data["content_block"];
                let block = match cb["type"].as_str() {
                    Some("thinking") => PartialBlock::Thinking {
                        text: String::new(),
                        signature: String::new(),
                    },
                    Some("tool_use") => PartialBlock::ToolUse {
                        id: cb["id"].as_str().unwrap_or_default().to_string(),
                        name: cb["name"].as_str().unwrap_or_default().to_string(),
                        partial_json: String::new(),
                    },
                    _ => PartialBlock::Text(String::new()),
                };
                while self.blocks.len() <= idx {
                    self.blocks.push(PartialBlock::default());
                }
                self.blocks[idx] = block;
            }
            Some("content_block_delta") => {
                let idx = data["index"].as_u64().unwrap_or(0) as usize;
                let delta = &data["delta"];
                if let Some(block) = self.blocks.get_mut(idx) {
                    match (block, delta["type"].as_str()) {
                        (PartialBlock::Text(t), Some("text_delta")) => {
                            let piece = delta["text"].as_str().unwrap_or_default();
                            t.push_str(piece);
                            events.push(StreamEvent::TextDelta(piece.to_string()));
                        }
                        (PartialBlock::Thinking { text, .. }, Some("thinking_delta")) => {
                            let piece = delta["thinking"].as_str().unwrap_or_default();
                            text.push_str(piece);
                            events.push(StreamEvent::ThinkingDelta(piece.to_string()));
                        }
                        (PartialBlock::Thinking { signature, .. }, Some("signature_delta")) => {
                            signature.push_str(delta["signature"].as_str().unwrap_or_default());
                        }
                        (PartialBlock::ToolUse { partial_json, .. }, Some("input_json_delta")) => {
                            partial_json
                                .push_str(delta["partial_json"].as_str().unwrap_or_default());
                        }
                        _ => {}
                    }
                }
            }
            Some("message_delta") => {
                if let Some(sr) = data["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(sr.to_string());
                }
                if let Some(out) = data["usage"]["output_tokens"].as_u64() {
                    self.usage.output_tokens = out;
                }
            }
            _ => {}
        }
        events
    }

    fn finish(self, model: &str) -> LlmResponse {
        let mut content = Vec::new();
        for block in self.blocks {
            match block {
                PartialBlock::Text(t) if t.is_empty() => {}
                PartialBlock::Text(t) => content.push(ContentBlock::Text(t)),
                PartialBlock::Thinking { text, signature } => {
                    content.push(ContentBlock::Thinking(text));
                    if !signature.is_empty() {
                        content.push(ContentBlock::ProviderState {
                            provider: PROVIDER_STATE.to_string(),
                            data: json!({
                                "type": THINKING_SIGNATURE_STATE,
                                "signature": signature,
                            }),
                        });
                    }
                }
                PartialBlock::ToolUse {
                    id,
                    name,
                    partial_json,
                } => {
                    let input: Value = serde_json::from_str(&partial_json)
                        .unwrap_or_else(|_| Value::Object(Default::default()));
                    content.push(ContentBlock::ToolCall { id, name, input });
                }
            }
        }
        let stop_reason = map_stop_reason(self.stop_reason.as_deref());
        let usage = map_usage(self.usage, model);
        LlmResponse {
            content,
            stop_reason,
            error_message: None,
            context_overflow: false,
            usage,
        }
    }
}

// --- Provider ---

pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    prompt_cache: bool,
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
            prompt_cache: false,
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn with_prompt_cache(mut self, enabled: bool) -> Self {
        self.prompt_cache = enabled;
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
    fn supports_images(&self) -> bool {
        true
    }

    async fn complete(&self, ctx: &Context, opts: &CompleteOpts) -> LlmResponse {
        let request = build_request_with_cache(ctx, opts, self.prompt_cache);

        let result = self
            .client
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
            let full = format!("API {status}: {body}");
            if is_context_overflow_error(&body) {
                return LlmResponse::context_overflow_error(full);
            }
            return LlmResponse::error(full);
        }

        let raw: AnthropicResponse = match resp.json().await {
            Err(e) => return LlmResponse::error(format!("parse error: {e}")),
            Ok(r) => r,
        };

        let content = raw
            .content
            .into_iter()
            .flat_map(AnthropicResponseBlock::into_contents)
            .collect();
        let stop_reason = map_stop_reason(raw.stop_reason.as_deref());
        let usage = map_usage(raw.usage, &opts.model);

        LlmResponse {
            content,
            stop_reason,
            error_message: None,
            context_overflow: false,
            usage,
        }
    }

    async fn stream(
        &self,
        ctx: &Context,
        opts: &CompleteOpts,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> LlmResponse {
        use eventsource_stream::Eventsource;
        use futures_util::StreamExt;

        let mut request = build_request_with_cache(ctx, opts, self.prompt_cache);
        request.stream = true;

        let result = self
            .client
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
            let full = format!("API {status}: {body}");
            if is_context_overflow_error(&body) {
                return LlmResponse::context_overflow_error(full);
            }
            return LlmResponse::error(full);
        }

        let mut events = resp.bytes_stream().eventsource();
        let mut state = StreamState::default();

        while let Some(event) = events.next().await {
            let event = match event {
                Ok(e) => e,
                Err(e) => return LlmResponse::error(format!("stream error: {e}")),
            };
            if event.event == "ping" {
                continue;
            }
            let data: Value = match serde_json::from_str(&event.data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if data["type"] == "error" {
                let msg = data["error"]["message"]
                    .as_str()
                    .unwrap_or("unknown stream error");
                let full = format!("API error: {msg}");
                if is_context_overflow_error(msg) {
                    return LlmResponse::context_overflow_error(full);
                }
                return LlmResponse::error(full);
            }
            for ev in state.on_data(&data) {
                on_event(ev);
            }
        }

        state.finish(&opts.model)
    }

    async fn context_window(&self, model: &str) -> Option<u64> {
        let resp = self
            .client
            .get(format!("{}/v1/models/{model}", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body: Value = resp.json().await.ok()?;
        max_input_tokens_from_model(&body).map(effective_context_window)
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
    fn stop_reason_refusal() {
        assert_eq!(map_stop_reason(Some("refusal")), StopReason::Refusal);
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
    fn map_usage_matches_canonical_semantics() {
        // Anthropic's input_tokens already EXCLUDES the cached portions, so
        // a plain pass-through satisfies the canonical Usage semantics
        // (ADR-002): prompt_tokens() = input + cache_read + cache_write =
        // the full window occupancy.
        let raw = AnthropicUsage {
            input_tokens: 1_000,
            output_tokens: 500,
            cache_read_input_tokens: 2_000,
            cache_creation_input_tokens: 100,
        };
        let u = map_usage(raw, "claude-opus-4-8");
        assert_eq!(u.prompt_tokens(), 3_100);
    }

    // --- context-overflow classification (ADR-002, vikunja #964) ---

    #[test]
    fn overflow_classifier_matches_anthropic_phrasings() {
        for msg in [
            "prompt is too long: 213462 tokens > 200000 maximum",
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"Prompt is too long"}}"#,
            "input length and `max_tokens` exceed context limit: 199000 + 8192 > 200000",
        ] {
            assert!(
                is_context_overflow_error(msg),
                "should classify as overflow: {msg}"
            );
        }
    }

    #[test]
    fn overflow_classifier_rejects_other_anthropic_errors() {
        for msg in [
            "rate limit exceeded",
            "invalid x-api-key",
            "overloaded_error: Overloaded",
            "max_tokens: must be greater than 0",
        ] {
            assert!(
                !is_context_overflow_error(msg),
                "must not classify as overflow: {msg}"
            );
        }
    }

    // --- context window (vikunja #965) ---

    #[test]
    fn max_input_tokens_parsed_from_model_body() {
        let body = json!({"id": "claude-opus-4-8", "max_input_tokens": 200000});
        assert_eq!(max_input_tokens_from_model(&body), Some(200_000));
    }

    #[test]
    fn max_input_tokens_absent_is_none() {
        let body = json!({"id": "claude-opus-4-8"});
        assert_eq!(max_input_tokens_from_model(&body), None);
    }

    #[test]
    fn max_input_tokens_zero_is_none() {
        let body = json!({"id": "claude-opus-4-8", "max_input_tokens": 0});
        assert_eq!(max_input_tokens_from_model(&body), None);
    }

    // --- effective_context_window (vikunja #1228) ---

    #[test]
    fn beta_ceiling_is_clamped_to_serving_window() {
        // The models endpoint advertises the 1M long-context beta ceiling;
        // without the `context-1m` beta header our requests cap at 200k.
        assert_eq!(effective_context_window(1_000_000), 200_000);
    }

    #[test]
    fn windows_at_or_below_the_cap_pass_through() {
        assert_eq!(effective_context_window(200_000), 200_000);
        assert_eq!(effective_context_window(150_000), 150_000);
    }

    #[tokio::test]
    async fn context_window_clamps_models_api_beta_ceiling() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                // Drain the request head before responding so the client never
                // sees the connection close with unread request bytes in
                // flight (a connection-reset flake on some platforms).
                use tokio::io::AsyncReadExt;
                let mut buf = [0u8; 1024];
                let mut head = Vec::new();
                while let Ok(n) = stream.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    head.extend_from_slice(&buf[..n]);
                    if head.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let body = r#"{"id":"claude-opus-5","max_input_tokens":1000000}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(resp.as_bytes()).await.ok();
            }
        });
        let provider =
            AnthropicProvider::new("k").with_base_url(format!("http://127.0.0.1:{port}"));
        // Regression for vikunja #1228: 1M-advertised models must resolve to
        // the real serving window or ADR-002 compaction never triggers.
        assert_eq!(
            provider.context_window("claude-opus-5").await,
            Some(200_000)
        );
    }

    #[tokio::test]
    async fn context_window_returns_none_on_http_error() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let resp =
                    b"HTTP/1.1 404 Not Found\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
                stream.write_all(resp).await.ok();
            }
        });
        let provider =
            AnthropicProvider::new("k").with_base_url(format!("http://127.0.0.1:{port}"));
        assert_eq!(provider.context_window("claude-opus-4-8").await, None);
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
        assert!(
            json.get("temperature").is_none(),
            "temperature must not appear"
        );
        assert!(json.get("top_p").is_none(), "top_p must not appear");
        assert!(json.get("top_k").is_none(), "top_k must not appear");
        assert!(
            json.get("budget_tokens").is_none(),
            "budget_tokens must not appear"
        );
    }

    #[test]
    fn build_request_serializes_base64_image_block() {
        let ctx = Context {
            messages: vec![Message {
                role: Role::User,
                content: vec![
                    ContentBlock::Text("describe this".into()),
                    ContentBlock::Image {
                        data: "aW1hZ2U=".into(),
                        media_type: "image/png".into(),
                        uri: Some("file:///tmp/image.png".into()),
                    },
                ],
            }],
            system: None,
            tools: vec![],
            stable_prefix_len: 0,
        };
        let json = serde_json::to_value(build_request(&ctx, &CompleteOpts::default())).unwrap();
        assert_eq!(
            json["messages"][0]["content"][1],
            json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": "image/png",
                    "data": "aW1hZ2U="
                }
            })
        );
    }

    #[test]
    fn provider_advertises_image_support() {
        assert!(AnthropicProvider::new("key").supports_images());
    }

    #[test]
    fn build_request_explicit_temperature_is_sent_when_thinking_off() {
        let ctx = Context {
            messages: vec![Message::user("hello")],
            system: None,
            tools: vec![],
            stable_prefix_len: 0,
        };
        let opts = CompleteOpts {
            thinking: ThinkingLevel::Off,
            temperature: Some(0.0),
            ..CompleteOpts::default()
        };
        let json = serde_json::to_value(build_request(&ctx, &opts)).unwrap();
        assert_eq!(json["temperature"], 0.0);
    }

    #[test]
    fn build_request_drops_temperature_when_thinking_enabled() {
        // Anthropic rejects a custom temperature alongside thinking; the
        // provider must drop it rather than send an invalid request.
        let ctx = Context {
            messages: vec![Message::user("hello")],
            system: None,
            tools: vec![],
            stable_prefix_len: 0,
        };
        let opts = CompleteOpts {
            model: "claude-opus-4-8".to_string(), // supports adaptive thinking
            temperature: Some(0.0),
            ..CompleteOpts::default()
        };
        let json = serde_json::to_value(build_request(&ctx, &opts)).unwrap();
        assert_eq!(json["thinking"]["type"], "adaptive");
        assert!(
            json.get("temperature").is_none(),
            "temperature must be dropped with thinking on"
        );
    }

    #[test]
    fn build_request_includes_adaptive_thinking_for_opus() {
        let ctx = Context {
            messages: vec![Message::user("hello")],
            system: None,
            tools: vec![],
            stable_prefix_len: 0,
        };
        let opts = CompleteOpts {
            model: "claude-opus-4-8".to_string(),
            ..CompleteOpts::default()
        };
        let json = serde_json::to_value(build_request(&ctx, &opts)).unwrap();
        assert_eq!(json["thinking"]["type"], "adaptive");
    }

    #[test]
    fn build_request_includes_adaptive_thinking_for_opus_5() {
        let ctx = Context {
            messages: vec![Message::user("hello")],
            system: None,
            tools: vec![],
            stable_prefix_len: 0,
        };
        let opts = CompleteOpts {
            model: "claude-opus-5".to_string(),
            ..CompleteOpts::default()
        };
        let json = serde_json::to_value(build_request(&ctx, &opts)).unwrap();
        assert_eq!(json["thinking"]["type"], "adaptive");
    }

    #[test]
    fn build_request_disables_default_thinking_for_opus_5() {
        let ctx = Context {
            messages: vec![Message::user("hello")],
            system: None,
            tools: vec![],
            stable_prefix_len: 0,
        };
        let opts = CompleteOpts {
            model: "claude-opus-5".to_string(),
            thinking: ThinkingLevel::Off,
            ..CompleteOpts::default()
        };
        let json = serde_json::to_value(build_request(&ctx, &opts)).unwrap();
        assert_eq!(json["thinking"]["type"], "disabled");
    }

    #[test]
    fn build_request_no_thinking_for_haiku() {
        let ctx = Context {
            messages: vec![Message::user("hello")],
            system: None,
            tools: vec![],
            stable_prefix_len: 0,
        };
        let opts = CompleteOpts {
            model: "claude-haiku-4-5".to_string(),
            ..CompleteOpts::default()
        };
        let json = serde_json::to_value(build_request(&ctx, &opts)).unwrap();
        assert!(json.get("thinking").is_none() || json["thinking"].is_null());
    }

    #[test]
    fn build_request_thinking_off_suppresses_thinking() {
        let ctx = Context {
            messages: vec![Message::user("hello")],
            system: None,
            tools: vec![],
            stable_prefix_len: 0,
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
            stable_prefix_len: 1, // first message is stable
        };
        let json = serde_json::to_value(build_request(&ctx, &CompleteOpts::default())).unwrap();
        let first_block = &json["messages"][0]["content"][0];
        assert_eq!(first_block["cache_control"]["type"], "ephemeral");
        // Second message (volatile) must not have cache_control
        let second_block = &json["messages"][1]["content"][0];
        assert!(
            second_block.get("cache_control").is_none() || second_block["cache_control"].is_null()
        );
    }

    #[test]
    fn provider_state_only_message_is_dropped() {
        let ctx = Context {
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ProviderState {
                    provider: "openai".into(),
                    data: json!({"type":"reasoning","encrypted_content":"opaque"}),
                }],
            }],
            system: None,
            tools: vec![],
            stable_prefix_len: 1,
        };
        let json = serde_json::to_value(build_request(&ctx, &CompleteOpts::default())).unwrap();
        assert!(json["messages"].as_array().unwrap().is_empty());
    }

    #[test]
    fn skipped_provider_state_does_not_steal_cache_boundary() {
        let ctx = Context {
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text("visible".into()),
                    ContentBlock::ProviderState {
                        provider: "openai".into(),
                        data: json!({"type":"reasoning","encrypted_content":"opaque"}),
                    },
                ],
            }],
            system: None,
            tools: vec![],
            stable_prefix_len: 1,
        };
        let json = serde_json::to_value(build_request(&ctx, &CompleteOpts::default())).unwrap();
        let content = json["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn build_request_no_cache_control_when_stable_prefix_len_zero() {
        let ctx = Context {
            messages: vec![Message::user("hello")],
            system: None,
            tools: vec![],
            stable_prefix_len: 0,
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

    #[test]
    fn build_request_caches_the_stable_tool_definition_prefix() {
        let ctx = Context {
            messages: vec![Message::user("go")],
            system: Some("stable system".into()),
            tools: vec![
                ToolSchema {
                    name: "read_file".into(),
                    description: "Read".into(),
                    input_schema: json!({"type":"object"}),
                },
                ToolSchema {
                    name: "write_file".into(),
                    description: "Write".into(),
                    input_schema: json!({"type":"object"}),
                },
            ],
            stable_prefix_len: 0,
        };

        let default_json =
            serde_json::to_value(build_request(&ctx, &CompleteOpts::default())).unwrap();
        assert!(default_json["tools"][0].get("cache_control").is_none());
        assert!(default_json["tools"][1].get("cache_control").is_none());

        let cached_json = serde_json::to_value(build_request_with_cache(
            &ctx,
            &CompleteOpts::default(),
            true,
        ))
        .unwrap();
        assert!(cached_json["tools"][0].get("cache_control").is_none());
        assert_eq!(
            cached_json["tools"][1]["cache_control"]["type"],
            "ephemeral"
        );
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

        let provider =
            AnthropicProvider::new("bad-key").with_base_url(format!("http://127.0.0.1:{port}"));
        let ctx = Context {
            messages: vec![Message::user("hello")],
            system: None,
            tools: vec![],
            stable_prefix_len: 0,
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
        }))
        .unwrap();
        assert!(
            matches!(&block.into_contents()[..], [ContentBlock::Text(t)] if t == "hello world")
        );
    }

    #[test]
    fn response_thinking_signature_is_preserved_for_replay() {
        let block: AnthropicResponseBlock = serde_json::from_value(json!({
            "type": "thinking",
            "thinking": "reasoning...",
            "signature": "signed-thinking"
        }))
        .unwrap();
        let content = block.into_contents();
        let ctx = Context {
            messages: vec![Message {
                role: Role::Assistant,
                content,
            }],
            system: None,
            tools: vec![],
            stable_prefix_len: 0,
        };
        let request = serde_json::to_value(build_request(&ctx, &CompleteOpts::default())).unwrap();

        assert_eq!(
            request["messages"][0]["content"][0]["signature"],
            "signed-thinking"
        );
    }

    #[test]
    fn response_block_tool_use_parses() {
        let block: AnthropicResponseBlock = serde_json::from_value(json!({
            "type": "tool_use",
            "id": "toolu_01",
            "name": "read_file",
            "input": {"path": "foo.txt"}
        }))
        .unwrap();
        assert!(
            matches!(&block.into_contents()[..], [ContentBlock::ToolCall { name, .. }] if name == "read_file")
        );
    }

    #[test]
    fn response_block_unknown_type_returns_none() {
        let block: AnthropicResponseBlock = serde_json::from_value(json!({
            "type": "redacted_thinking",
            "data": "opaque"
        }))
        .unwrap();
        assert!(block.into_contents().is_empty());
    }

    // --- StreamState (vikunja #957) ---

    #[test]
    fn stream_text_deltas_accumulate_and_emit() {
        let mut state = StreamState::default();
        state
            .on_data(&json!({"type": "message_start", "message": {"usage": {"input_tokens": 10}}}));
        state.on_data(&json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}));
        let e1 = state.on_data(&json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hel"}}));
        let e2 = state.on_data(&json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "lo"}}));
        assert_eq!(e1, vec![StreamEvent::TextDelta("hel".to_string())]);
        assert_eq!(e2, vec![StreamEvent::TextDelta("lo".to_string())]);
        state.on_data(&json!({"type": "content_block_stop", "index": 0}));
        state.on_data(&json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}}));
        let resp = state.finish("claude-haiku-4-5");
        assert!(matches!(&resp.content[0], ContentBlock::Text(t) if t == "hello"));
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert_eq!(resp.usage.input, 10);
        assert_eq!(resp.usage.output, 5);
    }

    #[test]
    fn stream_thinking_deltas_emit_and_accumulate() {
        let mut state = StreamState::default();
        state.on_data(&json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}));
        let ev = state.on_data(&json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "reasoning..."}}));
        assert_eq!(
            ev,
            vec![StreamEvent::ThinkingDelta("reasoning...".to_string())]
        );
        state.on_data(
            &json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {}}),
        );
        let resp = state.finish("claude-haiku-4-5");
        assert!(matches!(&resp.content[0], ContentBlock::Thinking(t) if t == "reasoning..."));
    }

    #[test]
    fn stream_preserves_thinking_signature_for_tool_loop_replay() {
        let mut state = StreamState::default();
        state.on_data(&json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "thinking", "thinking": ""}
        }));
        state.on_data(&json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "thinking_delta", "thinking": "reasoning..."}
        }));
        state.on_data(&json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "signature_delta", "signature": "signed-thinking"}
        }));
        state.on_data(&json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {}}
        }));
        state.on_data(&json!({
            "type": "content_block_delta",
            "index": 1,
            "delta": {"type": "input_json_delta", "partial_json": "{}"}
        }));
        state.on_data(
            &json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {}}),
        );

        let response = state.finish("claude-opus-5");
        let ctx = Context {
            messages: vec![
                Message {
                    role: Role::Assistant,
                    content: response.content,
                },
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "toolu_1".into(),
                        content: "{}".into(),
                        is_error: false,
                    }],
                },
            ],
            system: None,
            tools: vec![],
            stable_prefix_len: 0,
        };
        let request = serde_json::to_value(build_request(&ctx, &CompleteOpts::default())).unwrap();

        assert_eq!(
            request["messages"][0]["content"][0]["signature"],
            "signed-thinking"
        );
    }

    #[test]
    fn stream_tool_use_input_json_accumulates_silently() {
        let mut state = StreamState::default();
        state.on_data(&json!({
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {}}
        }));
        let ev1 = state.on_data(&json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"path\":"}}));
        let ev2 = state.on_data(&json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "\"a.txt\"}"}}));
        assert!(
            ev1.is_empty(),
            "tool input deltas must not be emitted as stream events"
        );
        assert!(ev2.is_empty());
        state.on_data(
            &json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {}}),
        );
        let resp = state.finish("claude-haiku-4-5");
        assert!(matches!(
            &resp.content[0],
            ContentBlock::ToolCall { id, name, input } if id == "toolu_1" && name == "read_file" && input["path"] == "a.txt"
        ));
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn stream_multiple_blocks_by_index() {
        let mut state = StreamState::default();
        state.on_data(&json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}));
        state.on_data(&json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "checking"}}));
        state.on_data(&json!({"type": "content_block_stop", "index": 0}));
        state.on_data(&json!({
            "type": "content_block_start", "index": 1,
            "content_block": {"type": "tool_use", "id": "t1", "name": "exec", "input": {}}
        }));
        state.on_data(&json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{}"}}));
        state.on_data(
            &json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {}}),
        );
        let resp = state.finish("claude-haiku-4-5");
        assert_eq!(resp.content.len(), 2);
        assert!(matches!(&resp.content[0], ContentBlock::Text(t) if t == "checking"));
        assert!(matches!(&resp.content[1], ContentBlock::ToolCall { name, .. } if name == "exec"));
    }

    #[test]
    fn stream_empty_text_block_omitted_from_content() {
        let mut state = StreamState::default();
        state.on_data(&json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}));
        state.on_data(
            &json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {}}),
        );
        let resp = state.finish("claude-haiku-4-5");
        assert!(
            resp.content.is_empty(),
            "an empty text block should not appear in content"
        );
    }
}
