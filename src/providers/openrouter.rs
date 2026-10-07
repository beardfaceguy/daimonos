use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::providers::{
    resolve_max_output, CompleteOpts, ContentBlock, Context, Cost, LlmProvider, LlmResponse,
    Message, Role, StopReason, StreamEvent, ThinkingLevel, ToolSchema, Usage,
};

/// `ContentBlock::ProviderState` owner tag for OpenRouter `reasoning_details`.
const PROVIDER_STATE: &str = "openrouter";

pub struct OpenRouterProvider {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
    prompt_cache: bool,
    request_trace: super::request_trace::RequestTrace,
    /// Bounded HTTP/SSE deadlines (#1107).
    timeouts: super::ProviderTimeouts,
    /// Per-model capabilities from the last successful `/models` fetch.
    /// Replaced wholesale on each fetch, so it is bounded by the catalog size.
    /// The request path only reads it and never fetches.
    catalog: StdMutex<HashMap<String, ModelCaps>>,
}

#[derive(Debug, Default, Clone, PartialEq)]
struct ModelCaps {
    max_output: Option<u32>,
    thinking_levels: Option<Vec<ThinkingLevel>>,
}

impl OpenRouterProvider {
    pub fn new(api_key: String, base_url: String) -> Result<Self, String> {
        // #1107: built through ProviderTimeouts so no client exists without a
        // connect deadline.
        let timeouts = super::ProviderTimeouts::default();
        let client = timeouts.client();
        Ok(Self {
            api_key,
            base_url,
            client,
            prompt_cache: false,
            request_trace: super::request_trace::RequestTrace::from_env(),
            timeouts,
            catalog: StdMutex::new(HashMap::new()),
        })
    }

    fn remember_catalog(&self, body: &Value) {
        let caps = caps_from_models(body);
        if !caps.is_empty() {
            *self.catalog.lock().unwrap_or_else(|p| p.into_inner()) = caps;
        }
    }

    fn cached_caps(&self, model: &str) -> Option<ModelCaps> {
        self.catalog
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(model)
            .cloned()
    }

    fn known_max_output(&self, model: &str) -> Option<u32> {
        self.cached_caps(model)?.max_output
    }

    /// Fetch `GET /models`, refreshing the capability cache on success.
    async fn fetch_catalog(&self) -> Option<Value> {
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .get(url)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body: Value = resp.json().await.ok()?;
        self.remember_catalog(&body);
        Some(body)
    }

    fn request_body(&self, ctx: &Context, opts: &CompleteOpts, stream: bool) -> Value {
        let mut messages = messages_to_wire(ctx.system.as_deref(), &ctx.messages);
        if self.prompt_cache && supports_explicit_prompt_cache(&opts.model) {
            apply_prompt_cache(&mut messages);
        }
        let tools = tools_to_wire(&ctx.tools);

        // No `stream_options`: OpenRouter deprecated `include_usage` (usage
        // is now always included in the final SSE chunk automatically).
        let mut body = json!({
            "model": opts.model,
            "messages": messages,
            // Reasoning budgets are carved out of max_tokens, so the default
            // must be the model's real output ceiling, not the generic floor.
            "max_tokens": resolve_max_output(
                opts.max_tokens,
                self.known_max_output(&opts.model),
                None,
            ),
            "reasoning": reasoning_param(&opts.thinking),
            "stream": stream,
        });

        if let Some(t) = opts.temperature {
            body["temperature"] = json!(t);
        }
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        body
    }

    pub fn with_prompt_cache(mut self, enabled: bool) -> Self {
        self.prompt_cache = enabled;
        self
    }

    /// Override the bounded HTTP/SSE deadlines (#1107).
    pub fn with_timeouts(mut self, timeouts: crate::providers::ProviderTimeouts) -> Self {
        self.client = timeouts.client();
        self.timeouts = timeouts;
        self
    }
}

#[async_trait]
impl LlmProvider for OpenRouterProvider {
    fn supports_images(&self) -> bool {
        true
    }

    async fn list_models(&self) -> Option<Vec<String>> {
        // OpenRouter serves hundreds of models across vendors; slugs sorted
        // newest-first by `created` so the failover chain degrades toward
        // older models. The picker is long, but discovery is the point.
        let body = self.fetch_catalog().await?;
        let mut entries: Vec<(i64, String)> = body["data"]
            .as_array()?
            .iter()
            .filter_map(|m| {
                Some((
                    m["created"].as_i64().unwrap_or(0),
                    m["id"].as_str()?.to_string(),
                ))
            })
            .collect();
        entries.sort_by_key(|(created, _)| std::cmp::Reverse(*created));
        let ids: Vec<String> = entries.into_iter().map(|(_, id)| id).collect();
        (!ids.is_empty()).then_some(ids)
    }

    async fn complete(&self, ctx: &Context, opts: &CompleteOpts) -> LlmResponse {
        let body = self.request_body(ctx, opts, false);
        self.request_trace.capture(&body).await;

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        // #1107: bound the wait for response headers; connect_timeout only
        // covers TCP/TLS establishment, not a server that accepts and stalls.
        let sent = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send();
        let sent = match super::with_deadline("response headers", self.timeouts.headers, sent).await
        {
            Ok(sent) => sent,
            Err(timeout) => return timeout,
        };
        let resp = match sent {
            Ok(r) => r,
            Err(e) => {
                return LlmResponse::retryable_error(format!("openrouter request failed: {e}"))
            }
        };

        let status = resp.status();
        let resp_body: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => return LlmResponse::error(format!("openrouter response parse: {e}")),
        };

        if !status.is_success() {
            let msg = resp_body["error"]["message"]
                .as_str()
                .unwrap_or("unknown error")
                .to_string();
            let full = format!("openrouter {status}: {msg}");
            if is_context_overflow_error(&msg) {
                return LlmResponse::context_overflow_error(full);
            }
            // #1240: transient upstream failure, classified provider-side.
            if super::is_retryable_status(status.as_u16()) {
                return LlmResponse::retryable_error(full);
            }
            return LlmResponse::error(full);
        }

        parse_response(&resp_body)
    }

    async fn stream(
        &self,
        ctx: &Context,
        opts: &CompleteOpts,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> LlmResponse {
        use eventsource_stream::Eventsource;
        use futures_util::StreamExt;

        let body = self.request_body(ctx, opts, true);
        self.request_trace.capture(&body).await;

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        // #1107: bound the wait for response headers; connect_timeout only
        // covers TCP/TLS establishment, not a server that accepts and stalls.
        let sent = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send();
        let sent = match super::with_deadline("response headers", self.timeouts.headers, sent).await
        {
            Ok(sent) => sent,
            Err(timeout) => return timeout,
        };
        let resp = match sent {
            Ok(r) => r,
            Err(e) => {
                return LlmResponse::retryable_error(format!("openrouter request failed: {e}"))
            }
        };

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            let full = format!("openrouter {status}: {body_text}");
            if is_context_overflow_error(&body_text) {
                return LlmResponse::context_overflow_error(full);
            }
            // #1240: transient upstream failure, classified provider-side.
            if super::is_retryable_status(status.as_u16()) {
                return LlmResponse::retryable_error(full);
            }
            return LlmResponse::error(full);
        }

        let mut events = resp.bytes_stream().eventsource();
        let mut state = StreamState::default();

        // #1107: see the anthropic adapter — first-event allowance, then a
        // resetting idle deadline.
        let mut first = true;
        loop {
            let limit = if first {
                self.timeouts.first_event
            } else {
                self.timeouts.idle
            };
            let phase = if first {
                "first stream event"
            } else {
                "stream idle"
            };
            let next = match super::with_deadline(phase, limit, events.next()).await {
                Ok(next) => next,
                Err(timeout) => return timeout,
            };
            first = false;
            let Some(event) = next else { break };
            let event = match event {
                Ok(e) => e,
                // #1418: a mid-stream socket break is transport, not phrasing —
                // classify it retryable so core resumes the turn (ADR-001).
                Err(e) => return super::stream_error_response("openrouter stream error", e),
            };
            if event.data == "[DONE]" {
                break;
            }
            let chunk: Value = match serde_json::from_str(&event.data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            for ev in state.on_chunk(&chunk) {
                on_event(ev);
            }
        }

        state.finish()
    }

    async fn context_window(&self, model: &str) -> Option<u64> {
        let body = self.fetch_catalog().await?;
        context_length_from_models(&body, model)
    }

    async fn thinking_levels(&self, model: &str) -> Option<Vec<ThinkingLevel>> {
        if let Some(caps) = self.cached_caps(model) {
            return caps.thinking_levels;
        }
        let body = self.fetch_catalog().await?;
        let entry = body["data"]
            .as_array()?
            .iter()
            .find(|entry| entry["id"].as_str() == Some(model))?;
        thinking_levels_from_model(entry)
    }
}

// --- Streaming (vikunja #957) ---

#[derive(Default, Clone)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Accumulates OpenAI-format streaming chunks (`choices[0].delta`) into the
/// same `LlmResponse` shape `parse_response` builds from one JSON body.
#[derive(Default)]
struct StreamState {
    text: String,
    thinking: String,
    reasoning_details: Vec<Value>,
    tool_calls: Vec<PartialToolCall>,
    finish_reason: Option<String>,
    usage: Value,
}

impl StreamState {
    /// Feed one decoded `data:` JSON chunk. Returns text deltas to forward
    /// live; tool-call arguments accumulate silently.
    fn on_chunk(&mut self, chunk: &Value) -> Vec<StreamEvent> {
        let mut events = Vec::new();

        if let Some(u) = chunk.get("usage") {
            if !u.is_null() {
                self.usage = u.clone();
            }
        }

        let choice = &chunk["choices"][0];
        if let Some(fr) = choice["finish_reason"].as_str() {
            self.finish_reason = Some(fr.to_string());
        }

        let delta = &choice["delta"];
        if let Some(piece) = delta["reasoning"].as_str() {
            if !piece.is_empty() {
                self.thinking.push_str(piece);
                events.push(StreamEvent::ThinkingDelta(piece.to_string()));
            }
        }
        if let Some(details) = delta["reasoning_details"].as_array() {
            for fragment in details {
                merge_reasoning_detail(&mut self.reasoning_details, fragment);
            }
        }
        if let Some(piece) = delta["content"].as_str() {
            if !piece.is_empty() {
                self.text.push_str(piece);
                events.push(StreamEvent::TextDelta(piece.to_string()));
            }
        }

        if let Some(tool_calls) = delta["tool_calls"].as_array() {
            for tc in tool_calls {
                let idx = tc["index"].as_u64().unwrap_or(0) as usize;
                while self.tool_calls.len() <= idx {
                    self.tool_calls.push(PartialToolCall::default());
                }
                let slot = &mut self.tool_calls[idx];
                if let Some(id) = tc["id"].as_str() {
                    slot.id = id.to_string();
                }
                if let Some(name) = tc["function"]["name"].as_str() {
                    slot.name = name.to_string();
                }
                if let Some(args) = tc["function"]["arguments"].as_str() {
                    slot.arguments.push_str(args);
                }
            }
        }

        events
    }

    fn finish(self) -> LlmResponse {
        let mut content = reasoning_blocks(&self.thinking, self.reasoning_details);
        if !self.text.is_empty() {
            content.push(ContentBlock::Text(self.text));
        }
        for tc in self.tool_calls {
            let input: Value = serde_json::from_str(&tc.arguments)
                .unwrap_or_else(|_| Value::Object(Default::default()));
            content.push(ContentBlock::ToolCall {
                id: tc.id,
                name: tc.name,
                input,
            });
        }
        let stop_reason = map_finish_reason(self.finish_reason.as_deref());
        let usage = parse_usage(&self.usage);
        LlmResponse {
            retryable: false,
            content,
            stop_reason,
            error_message: None,
            context_overflow: false,
            usage,
        }
    }
}

// --- Pure helpers (pub(crate) for testability) ---

/// Serialize our internal messages to the OpenAI wire format.
/// System prompt (if any) is prepended as a system-role message.
/// A user `Message` containing `ToolResult` blocks expands into one
/// `role: "tool"` message per result (OpenAI convention).
pub(crate) fn messages_to_wire(system: Option<&str>, messages: &[Message]) -> Vec<Value> {
    let mut wire: Vec<Value> = Vec::new();

    if let Some(sys) = system {
        wire.push(json!({"role": "system", "content": sys}));
    }

    for msg in messages {
        match msg.role {
            Role::User => {
                let tool_results: Vec<_> = msg
                    .content
                    .iter()
                    .filter_map(|b| {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } = b
                        {
                            Some((tool_use_id.as_str(), content.as_str()))
                        } else {
                            None
                        }
                    })
                    .collect();

                if !tool_results.is_empty() {
                    for (id, content) in tool_results {
                        wire.push(json!({
                            "role": "tool",
                            "tool_call_id": id,
                            "content": content,
                        }));
                    }
                } else if msg
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Image { .. }))
                {
                    let content: Vec<Value> = msg
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(text) => Some(json!({"type": "text", "text": text})),
                            ContentBlock::Image {
                                data, media_type, ..
                            } => Some(json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:{media_type};base64,{data}")
                                }
                            })),
                            _ => None,
                        })
                        .collect();
                    wire.push(json!({"role": "user", "content": content}));
                } else {
                    let text: String = msg
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
                    wire.push(json!({"role": "user", "content": text}));
                }
            }
            Role::Assistant => {
                let mut text_parts: Vec<&str> = Vec::new();
                let mut tool_calls: Vec<Value> = Vec::new();
                let mut reasoning_details: Vec<Value> = Vec::new();

                for block in &msg.content {
                    match block {
                        ContentBlock::Text(t) => text_parts.push(t.as_str()),
                        ContentBlock::ToolCall { id, name, input } => {
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    // Arguments must be a JSON string, not an object
                                    "arguments": input.to_string(),
                                }
                            }));
                        }
                        ContentBlock::ProviderState { provider, data }
                            if provider == PROVIDER_STATE =>
                        {
                            if let Some(details) = data.as_array() {
                                reasoning_details.extend(details.iter().cloned());
                            }
                        }
                        // Display-only thinking and other providers' state are
                        // never replayed; `reasoning_details` carries continuity.
                        ContentBlock::Thinking(_) | ContentBlock::ProviderState { .. } => {}
                        // Images are only valid in user prompts.
                        ContentBlock::Image { .. } => {}
                        // Tool results belong on user messages, not assistant
                        ContentBlock::ToolResult { .. } => {}
                    }
                }

                let content = if text_parts.is_empty() {
                    Value::Null
                } else {
                    Value::String(text_parts.join("\n"))
                };

                let mut obj = json!({"role": "assistant", "content": content});
                if !tool_calls.is_empty() {
                    obj["tool_calls"] = json!(tool_calls);
                }
                if !reasoning_details.is_empty() {
                    obj["reasoning_details"] = json!(reasoning_details);
                }
                wire.push(obj);
            }
        }
    }

    wire
}

/// Place one Anthropic-compatible ephemeral cache breakpoint on the final
/// cacheable content block. This avoids the upstream-routing restriction of
/// OpenRouter's top-level automatic cache control.
fn apply_prompt_cache(messages: &mut [Value]) {
    let Some(message) = messages.last_mut() else {
        return;
    };
    let marker = json!({"type": "ephemeral"});

    match message.get_mut("content") {
        Some(Value::String(text)) => {
            let text = std::mem::take(text);
            message["content"] = json!([{
                "type": "text",
                "text": text,
                "cache_control": marker,
            }]);
        }
        Some(Value::Array(blocks)) => {
            if let Some(block) = blocks.last_mut() {
                block["cache_control"] = marker;
            }
        }
        _ => {}
    }
}

fn supports_explicit_prompt_cache(model: &str) -> bool {
    // Conservative initial scope: this wire extension is documented for
    // Anthropic-compatible requests. Widen only with provider-backed tests.
    model.starts_with("anthropic/")
}

/// Serialize tool schemas to the OpenAI `tools` array format.
pub(crate) fn tools_to_wire(tools: &[ToolSchema]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    // OpenRouter's OpenAI routes have produced different tool
                    // calls for omission versus explicit false, synthesizing
                    // every property only in the omitted case. Pin non-strict
                    // mode so `required` remains authoritative (#1526).
                    "strict": false,
                    "parameters": t.input_schema,
                }
            })
        })
        .collect()
}

/// OpenRouter's unified `reasoning` request object. Level names match
/// OpenRouter's effort vocabulary one-to-one; OpenRouter normalizes them per
/// upstream (Anthropic budgets, OpenAI effort, Gemini thinkingLevel).
fn reasoning_param(level: &ThinkingLevel) -> Value {
    match level {
        ThinkingLevel::Off => json!({"enabled": false}),
        level => json!({"effort": level.as_str()}),
    }
}

/// Fold one streamed `reasoning_details` fragment into the accumulated list.
/// Fragments sharing an `index` are one detail: string payloads concatenate
/// and the remaining fields (signature, id, format, type) take the latest
/// non-null value.
fn merge_reasoning_detail(details: &mut Vec<Value>, fragment: &Value) {
    let Some(fields) = fragment.as_object() else {
        return;
    };
    let index = fragment.get("index").and_then(Value::as_u64);
    let existing = index.and_then(|index| {
        details
            .iter_mut()
            .find(|detail| detail.get("index").and_then(Value::as_u64) == Some(index))
    });
    let Some(Value::Object(target)) = existing else {
        details.push(fragment.clone());
        return;
    };
    for (key, value) in fields {
        match (key.as_str(), value) {
            (_, Value::Null) => {}
            ("text" | "summary" | "data", Value::String(piece)) => match target.get_mut(key) {
                Some(Value::String(accumulated)) => accumulated.push_str(piece),
                _ => {
                    target.insert(key.clone(), value.clone());
                }
            },
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

/// Display text first, then the opaque replay state, matching the order the
/// provider produced them.
fn reasoning_blocks(thinking: &str, details: Vec<Value>) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();
    if !thinking.is_empty() {
        blocks.push(ContentBlock::Thinking(thinking.to_string()));
    }
    if !details.is_empty() {
        blocks.push(ContentBlock::ProviderState {
            provider: PROVIDER_STATE.to_string(),
            data: Value::Array(details),
        });
    }
    blocks
}

/// Parse an OpenRouter (OpenAI-format) response body into our neutral types.
pub(crate) fn parse_response(body: &Value) -> LlmResponse {
    let choice = &body["choices"][0];
    let finish_reason = choice["finish_reason"].as_str();
    let stop_reason = map_finish_reason(finish_reason);

    let usage = parse_usage(&body["usage"]);
    let message = &choice["message"];

    let mut content = reasoning_blocks(
        message["reasoning"].as_str().unwrap_or_default(),
        message["reasoning_details"]
            .as_array()
            .cloned()
            .unwrap_or_default(),
    );

    if let Some(text) = message["content"].as_str() {
        if !text.is_empty() {
            content.push(ContentBlock::Text(text.to_string()));
        }
    }

    if let Some(tool_calls) = message["tool_calls"].as_array() {
        for tc in tool_calls {
            let id = tc["id"].as_str().unwrap_or("").to_string();
            let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
            let args_str = tc["function"]["arguments"].as_str().unwrap_or("{}");
            let input: Value = serde_json::from_str(args_str)
                .unwrap_or_else(|_| Value::Object(Default::default()));
            content.push(ContentBlock::ToolCall { id, name, input });
        }
    }
    let error_message = (stop_reason == StopReason::Refusal).then(|| {
        message["refusal"]
            .as_str()
            .unwrap_or("provider content policy refusal")
            .to_string()
    });

    LlmResponse {
        retryable: false,
        content,
        stop_reason,
        error_message,
        context_overflow: false,
        usage,
    }
}

pub(crate) fn map_finish_reason(reason: Option<&str>) -> StopReason {
    match reason {
        Some("stop") | Some("end_turn") => StopReason::EndTurn,
        Some("tool_calls") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("content_filter") | Some("refusal") => StopReason::Refusal,
        _ => StopReason::Error,
    }
}

fn parse_usage(usage: &Value) -> Usage {
    // OpenAI-format `prompt_tokens` INCLUDES the cached portions; the cache
    // counts are sub-details under `prompt_tokens_details`. Canonical `Usage`
    // semantics (ADR-002) want `input` = NON-cached prompt tokens, so
    // subtract both details out — keeping the invariant
    // `Usage::prompt_tokens() == wire prompt_tokens` exact. When the details
    // are null/absent (vLLM, Ollama, most self-hosted), input = prompt_tokens
    // and the invariant still holds.
    let prompt = usage["prompt_tokens"].as_u64().unwrap_or(0);
    let details = &usage["prompt_tokens_details"];
    let cache_read = details["cached_tokens"].as_u64().unwrap_or(0);
    let cache_write = details["cache_write_tokens"].as_u64().unwrap_or(0);
    Usage {
        input: prompt
            .saturating_sub(cache_read)
            .saturating_sub(cache_write),
        output: usage["completion_tokens"].as_u64().unwrap_or(0),
        reasoning_output: usage["completion_tokens_details"]["reasoning_tokens"].as_u64(),
        thinking_bytes: 0,
        cache_read,
        cache_write,
        // OpenRouter reports the total account charge in the final usage frame
        // for both streaming and non-streaming responses. It does not split
        // that charge into our neutral input/output/cache buckets, so preserve
        // the authoritative total and leave those optional components at zero.
        cost: Cost {
            total_usd: usage["cost"]
                .as_f64()
                .filter(|cost| cost.is_finite() && *cost >= 0.0)
                .unwrap_or(0.0),
            ..Cost::default()
        },
    }
}

/// Classify an OpenAI-dialect error body/message as a context-window
/// overflow (ADR-002 reactive compaction). Provider-local knowledge, per
/// ADR-001. Phrasings vary by upstream: OpenAI/vLLM/xAI say "maximum
/// context length", Anthropic-through-OpenRouter passes through "prompt is
/// too long", llama.cpp says "exceeds the available context", Bedrock-style
/// says "input is too long".
pub(crate) fn is_context_overflow_error(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("maximum context length")
        || m.contains("context length exceeded")
        || m.contains("prompt is too long")
        || m.contains("exceeds the available context")
        || m.contains("input is too long")
}

/// Find `model`'s context window in an OpenAI-dialect `GET /models` body
/// (vikunja #965). OpenRouter reports it as `context_length`; self-hosted
/// vLLM exposes `max_model_len` on the same list endpoint, so fall back to
/// that. `None` when the model id isn't listed, both fields are absent, or
/// the value is zero.
/// Per-model capabilities from an OpenRouter `GET /models` body.
fn caps_from_models(body: &Value) -> HashMap<String, ModelCaps> {
    body["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let id = model["id"].as_str()?;
            let max_output = model["top_provider"]["max_completion_tokens"]
                .as_u64()
                .filter(|&n| n > 0)
                .map(|n| u32::try_from(n).unwrap_or(u32::MAX));
            let caps = ModelCaps {
                max_output,
                thinking_levels: thinking_levels_from_model(model),
            };
            Some((id.to_string(), caps))
        })
        .collect()
}

/// Levels a catalog entry honors, per OpenRouter's `reasoning` metadata:
/// `supported_efforts` lists the accepted efforts (`null` = all, omitted = no
/// effort selection), and `mandatory` models reject disabling reasoning.
/// Off is sent as `enabled: false`, so it is offered whenever reasoning is
/// optional even if `"none"` is not a listed effort.
fn thinking_levels_from_model(model: &Value) -> Option<Vec<ThinkingLevel>> {
    let reasoning = model.get("reasoning")?;
    let efforts = reasoning.get("supported_efforts")?;
    let mandatory = reasoning["mandatory"].as_bool().unwrap_or(false);
    let accepted: Vec<ThinkingLevel> = match efforts {
        Value::Null => ThinkingLevel::ALL.to_vec(),
        Value::Array(names) => names
            .iter()
            .filter_map(Value::as_str)
            .filter_map(|name| match name {
                "none" => Some(ThinkingLevel::Off),
                name => ThinkingLevel::from_input(name).ok(),
            })
            .collect(),
        _ => return None,
    };
    let levels: Vec<ThinkingLevel> = ThinkingLevel::ALL
        .into_iter()
        .filter(|level| match level {
            ThinkingLevel::Off => !mandatory,
            level => accepted.contains(level),
        })
        .collect();
    (levels.iter().any(|level| *level != ThinkingLevel::Off)).then_some(levels)
}

pub(crate) fn context_length_from_models(body: &Value, model: &str) -> Option<u64> {
    let entry = body["data"]
        .as_array()?
        .iter()
        .find(|m| m["id"].as_str() == Some(model))?;
    entry["context_length"]
        .as_u64()
        .or_else(|| entry["max_model_len"].as_u64())
        .filter(|&n| n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::test_support::mock_http_server;
    use serde_json::json;

    #[tokio::test]
    async fn complete_structural_trace_matches_http_request() {
        structural_trace_http_case(false, true, false).await;
    }

    #[tokio::test]
    async fn stream_structural_trace_matches_http_request() {
        structural_trace_http_case(true, true, false).await;
    }

    #[tokio::test]
    async fn complete_structural_trace_disabled_and_failure_are_fail_open() {
        structural_trace_http_case(false, false, false).await;
        structural_trace_http_case(false, true, true).await;
    }

    #[tokio::test]
    async fn stream_structural_trace_disabled_and_failure_are_fail_open() {
        structural_trace_http_case(true, false, false).await;
        structural_trace_http_case(true, true, true).await;
    }

    async fn structural_trace_http_case(streaming: bool, enabled: bool, broken: bool) {
        let dir = tempfile::tempdir().unwrap();
        let trace_dir = dir.path().join("traces");
        if broken {
            std::fs::write(&trace_dir, "not a directory").unwrap();
        }
        let terminal = json!({
            "choices": [{"delta": {"content": "ok"},
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2}
        });
        let (mime, response_body) = if streaming {
            (
                "text/event-stream",
                format!("data: {terminal}\n\ndata: [DONE]\n\n"),
            )
        } else {
            ("application/json", terminal.to_string())
        };
        let (base_url, captured) = mock_http_server("200 OK", mime, response_body).await;
        let mut provider = OpenRouterProvider::new("PRIVATE_KEY".into(), base_url.clone()).unwrap();
        // Inject opt-in state per provider; never mutate the process environment.
        provider.request_trace.directory = enabled.then(|| trace_dir.clone());
        let ctx = Context {
            system: Some("PRIVATE_SYSTEM".into()),
            messages: vec![
                Message::user("PRIVATE_PROMPT"),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolCall {
                        id: "call_1".into(),
                        name: "lookup".into(),
                        input: json!({"query": "PRIVATE_ARGUMENT"}),
                    }],
                },
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "call_1".into(),
                        content: "PRIVATE_RESULT".into(),
                        is_error: false,
                    }],
                },
            ],
            tools: vec![ToolSchema {
                name: "lookup".into(),
                description: "schema description retained".into(),
                input_schema: json!({
                    "type": "object", "additionalProperties": false,
                    "properties": {
                        "query": {"type": "string"},
                        "customView": {"type": "string", "minLength": 1,
                            "default": "example-view", "examples": ["view"]}
                    }, "required": ["query"]
                }),
            }],
            stable_prefix_len: 0,
        };
        let opts = CompleteOpts {
            model: "mock/model".into(),
            ..CompleteOpts::default()
        };
        let mut events = Vec::new();
        let response = if streaming {
            provider
                .stream(&ctx, &opts, &mut |event| events.push(event))
                .await
        } else {
            provider.complete(&ctx, &opts).await
        };
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        assert_eq!(response.usage.input, 10);
        assert_eq!(response.usage.output, 2);
        assert!(matches!(response.content.as_slice(), [ContentBlock::Text(text)] if text == "ok"));
        if streaming {
            assert!(!events.is_empty());
        }
        let request = captured.await.unwrap();
        for secret in [
            "PRIVATE_KEY",
            "PRIVATE_SYSTEM",
            "PRIVATE_PROMPT",
            "PRIVATE_ARGUMENT",
            "PRIVATE_RESULT",
        ] {
            assert!(request.contains(secret));
        }
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        if enabled && !broken {
            let text = std::fs::read_to_string(trace_dir.join("requests.jsonl")).unwrap();
            assert_eq!(text.lines().count(), 1);
            let trace: Value = serde_json::from_str(text.trim()).unwrap();
            assert_eq!(trace["tools"], body["tools"]);
            assert_eq!(
                trace["tools"][0]["function"]["parameters"]["required"],
                json!(["query"])
            );
            assert_eq!(
                trace["message_count"],
                body["messages"].as_array().unwrap().len()
            );
            assert_eq!(trace["model"], opts.model);
            assert_eq!(trace["stream"], streaming);
            uuid::Uuid::parse_str(trace["request_id"].as_str().unwrap()).unwrap();
            assert!(trace["unix_ms"].as_u64().unwrap() > 0);
            for secret in [
                "PRIVATE_KEY",
                "PRIVATE_SYSTEM",
                "PRIVATE_PROMPT",
                "PRIVATE_ARGUMENT",
                "PRIVATE_RESULT",
                &base_url,
            ] {
                assert!(!text.contains(secret));
            }
            assert_eq!(trace.as_object().unwrap().len(), 8);
        } else if broken {
            assert_eq!(
                std::fs::read_to_string(&trace_dir).unwrap(),
                "not a directory"
            );
        } else {
            assert!(!trace_dir.exists());
        }
    }

    #[tokio::test]
    async fn stream_prompt_cache_marks_latest_tool_result() {
        let terminal = json!({
            "choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2}
        });
        let sse = format!("data: {terminal}\n\ndata: [DONE]\n\n");
        let (base_url, captured) = mock_http_server("200 OK", "text/event-stream", sse).await;
        let provider = OpenRouterProvider::new("key".into(), base_url)
            .unwrap()
            .with_prompt_cache(true);
        let ctx = Context {
            messages: vec![
                Message::user("inspect"),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolCall {
                        id: "call_1".into(),
                        name: "read_file".into(),
                        input: json!({"path": "src/main.rs"}),
                    }],
                },
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "call_1".into(),
                        content: "file contents".into(),
                        is_error: false,
                    }],
                },
            ],
            system: Some("system".into()),
            tools: vec![],
            stable_prefix_len: 0,
        };

        let mut events = Vec::new();
        let options = CompleteOpts {
            model: "anthropic/claude-opus-4.8".into(),
            ..CompleteOpts::default()
        };
        let response = provider
            .stream(&ctx, &options, &mut |event| events.push(event))
            .await;

        assert_eq!(response.stop_reason, StopReason::EndTurn);
        let request = captured.await.unwrap();
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.last().unwrap()["role"], "tool");
        assert_eq!(
            messages.last().unwrap()["content"][0],
            json!({
                "type": "text",
                "text": "file contents",
                "cache_control": {"type": "ephemeral"}
            })
        );
        assert!(messages.last().unwrap().get("cache_control").is_none());
        assert_eq!(request.matches("\"cache_control\"").count(), 1);
    }

    #[tokio::test]
    async fn complete_prompt_cache_marks_latest_text_message() {
        let response = json!({
            "choices": [{
                "message": {"role": "assistant", "content": "done"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2}
        })
        .to_string();
        let (base_url, captured) = mock_http_server("200 OK", "application/json", response).await;
        let provider = OpenRouterProvider::new("key".into(), base_url)
            .unwrap()
            .with_prompt_cache(true);
        let ctx = Context {
            messages: vec![Message::user("inspect")],
            system: Some("system".into()),
            tools: vec![],
            stable_prefix_len: 0,
        };

        let options = CompleteOpts {
            model: "anthropic/claude-opus-4.8".into(),
            ..CompleteOpts::default()
        };
        let response = provider.complete(&ctx, &options).await;

        assert_eq!(response.stop_reason, StopReason::EndTurn);
        let request = captured.await.unwrap();
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(
            body["messages"][1]["content"][0],
            json!({
                "type": "text",
                "text": "inspect",
                "cache_control": {"type": "ephemeral"}
            })
        );
        assert_eq!(request.matches("\"cache_control\"").count(), 1);
    }

    #[tokio::test]
    async fn prompt_cache_does_not_mark_non_anthropic_openrouter_models() {
        let response = json!({
            "choices": [{
                "message": {"role": "assistant", "content": "done"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2}
        })
        .to_string();
        let (base_url, captured) = mock_http_server("200 OK", "application/json", response).await;
        let provider = OpenRouterProvider::new("key".into(), base_url)
            .unwrap()
            .with_prompt_cache(true);
        let ctx = Context {
            messages: vec![Message::user("inspect")],
            system: Some("system".into()),
            tools: vec![],
            stable_prefix_len: 0,
        };
        let options = CompleteOpts {
            model: "openai/gpt-5".into(),
            ..CompleteOpts::default()
        };

        let response = provider.complete(&ctx, &options).await;

        assert_eq!(response.stop_reason, StopReason::EndTurn);
        let request = captured.await.unwrap();
        assert!(!request.contains("\"cache_control\""));
    }

    #[test]
    fn prompt_cache_skips_messages_without_cacheable_content() {
        for content in [Value::Null, json!([])] {
            let mut messages = vec![json!({
                "role": "assistant",
                "content": content,
                "tool_calls": [{"id": "call_1"}]
            })];

            apply_prompt_cache(&mut messages);

            assert!(!messages[0].to_string().contains("cache_control"));
        }
    }

    /// vikunja #1418: the OpenRouter adapter's mid-stream read-error path must
    /// classify a broken transport (HTTP/2 CANCEL) retryable so the turn resumes.
    #[test]
    fn openrouter_mid_stream_transport_break_is_retryable() {
        let broken = eventsource_stream::EventStreamError::<String>::Transport(
            "http/2 stream closed with error code CANCEL (0x8)".to_string(),
        );
        let resp = crate::providers::stream_error_response("openrouter stream error", broken);
        assert!(resp.retryable);
        assert!(resp
            .error_message
            .as_deref()
            .is_some_and(|m| m.contains("openrouter stream error") && m.contains("CANCEL")));
    }

    // --- messages_to_wire ---

    #[test]
    fn wire_user_text_message() {
        let msgs = vec![Message::user("hello")];
        let wire = messages_to_wire(None, &msgs);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0]["role"], "user");
        assert_eq!(wire[0]["content"], "hello");
    }

    #[test]
    fn wire_user_image_message_uses_openai_multimodal_format() {
        let msgs = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text("describe this".into()),
                ContentBlock::Image {
                    data: "aW1hZ2U=".into(),
                    media_type: "image/png".into(),
                    uri: Some("file:///tmp/image.png".into()),
                },
            ],
        }];
        let wire = messages_to_wire(None, &msgs);
        assert_eq!(
            wire[0]["content"][0],
            json!({"type": "text", "text": "describe this"})
        );
        assert_eq!(
            wire[0]["content"][1],
            json!({
                "type": "image_url",
                "image_url": {"url": "data:image/png;base64,aW1hZ2U="}
            })
        );
    }

    #[test]
    fn provider_advertises_image_support() {
        let provider = OpenRouterProvider::new("key".into(), "https://example.com".into()).unwrap();
        assert!(provider.supports_images());
    }

    #[test]
    fn wire_system_prepended() {
        let msgs = vec![Message::user("hi")];
        let wire = messages_to_wire(Some("you are helpful"), &msgs);
        assert_eq!(wire.len(), 2);
        assert_eq!(wire[0]["role"], "system");
        assert_eq!(wire[0]["content"], "you are helpful");
        assert_eq!(wire[1]["role"], "user");
    }

    #[test]
    fn wire_assistant_text_message() {
        let msgs = vec![Message::assistant("the answer is 42")];
        let wire = messages_to_wire(None, &msgs);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0]["role"], "assistant");
        assert_eq!(wire[0]["content"], "the answer is 42");
        assert!(wire[0].get("tool_calls").is_none() || wire[0]["tool_calls"].is_null());
    }

    #[test]
    fn wire_tool_call_goes_to_tool_calls_array() {
        let msgs = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                input: json!({"path": "src/main.rs"}),
            }],
        }];
        let wire = messages_to_wire(None, &msgs);
        assert_eq!(wire[0]["role"], "assistant");
        assert!(wire[0]["content"].is_null());
        let tc = &wire[0]["tool_calls"][0];
        assert_eq!(tc["id"], "call_1");
        assert_eq!(tc["type"], "function");
        assert_eq!(tc["function"]["name"], "read_file");
        // arguments must be a JSON string
        let args: Value =
            serde_json::from_str(tc["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["path"], "src/main.rs");
    }

    #[test]
    fn wire_tool_result_becomes_tool_role_message() {
        let msgs = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "file contents here".into(),
                is_error: false,
            }],
        }];
        let wire = messages_to_wire(None, &msgs);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0]["role"], "tool");
        assert_eq!(wire[0]["tool_call_id"], "call_1");
        assert_eq!(wire[0]["content"], "file contents here");
    }

    #[test]
    fn wire_multiple_tool_results_expand_to_separate_messages() {
        let msgs = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "result A".into(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "call_2".into(),
                    content: "result B".into(),
                    is_error: false,
                },
            ],
        }];
        let wire = messages_to_wire(None, &msgs);
        assert_eq!(wire.len(), 2);
        assert_eq!(wire[0]["tool_call_id"], "call_1");
        assert_eq!(wire[1]["tool_call_id"], "call_2");
    }

    #[test]
    fn cancelled_multi_call_history_pairs_completed_and_unresolved_calls() {
        let messages = crate::agent::materialize_cancelled_suffix(
            vec![
                Message::user("run both"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::ProviderState {
                            provider: "openai".into(),
                            data: json!({"type":"reasoning","encrypted_content":"opaque"}),
                        },
                        ContentBlock::ToolCall {
                            id: "c1".into(),
                            name: "read_file".into(),
                            input: json!({"path":"a"}),
                        },
                        ContentBlock::ToolCall {
                            id: "c2".into(),
                            name: "exec".into(),
                            input: json!({"command":"work"}),
                        },
                    ],
                },
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "c1".into(),
                        content: "ok".into(),
                        is_error: false,
                    }],
                },
            ],
            "cancelled; side effects uncertain",
        );
        let wire = messages_to_wire(None, &messages);
        let assistant = wire
            .iter()
            .find(|message| message["tool_calls"].is_array())
            .expect("assistant tool-call message");
        assert_eq!(assistant["tool_calls"].as_array().unwrap().len(), 2);
        for id in ["c1", "c2"] {
            assert_eq!(
                wire.iter()
                    .filter(|message| message["role"] == "tool" && message["tool_call_id"] == id)
                    .count(),
                1
            );
        }
        assert!(wire.iter().any(|message| {
            message["role"] == "tool"
                && message["tool_call_id"] == "c2"
                && message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("side effects uncertain"))
        }));
    }

    #[test]
    fn wire_assistant_with_text_and_tool_call() {
        let msgs = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text("let me check that".into()),
                ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "exec".into(),
                    input: json!({"command": "ls"}),
                },
            ],
        }];
        let wire = messages_to_wire(None, &msgs);
        assert_eq!(wire[0]["content"], "let me check that");
        assert_eq!(wire[0]["tool_calls"][0]["id"], "c1");
    }

    #[test]
    fn wire_thinking_blocks_are_skipped() {
        let msgs = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking("internal reasoning".into()),
                ContentBlock::Text("visible reply".into()),
            ],
        }];
        let wire = messages_to_wire(None, &msgs);
        assert_eq!(wire[0]["content"], "visible reply");
        // No trace of the thinking block
        assert!(!wire[0].to_string().contains("internal reasoning"));
    }

    // --- tools_to_wire ---

    #[test]
    fn tools_to_wire_correct_shape() {
        let tools = vec![ToolSchema {
            name: "read_file".into(),
            description: "Read a file".into(),
            input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }];
        let wire = tools_to_wire(&tools);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0]["type"], "function");
        assert_eq!(wire[0]["function"]["name"], "read_file");
        assert_eq!(wire[0]["function"]["description"], "Read a file");
        assert_eq!(wire[0]["function"]["strict"], false);
        assert_eq!(wire[0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn tools_to_wire_empty_produces_empty() {
        assert!(tools_to_wire(&[]).is_empty());
    }

    // --- parse_response ---

    #[test]
    fn parse_end_turn_response() {
        let body = json!({
            "choices": [{"message": {"role": "assistant", "content": "done"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 50}
        });
        let resp = parse_response(&body);
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert!(matches!(&resp.content[0], ContentBlock::Text(t) if t == "done"));
    }

    #[test]
    fn parse_tool_use_response() {
        let body = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": {"name": "exec", "arguments": "{\"command\":\"ls\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 200, "completion_tokens": 30}
        });
        let resp = parse_response(&body);
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        assert!(matches!(
            &resp.content[0],
            ContentBlock::ToolCall { id, name, .. } if id == "call_abc" && name == "exec"
        ));
        if let ContentBlock::ToolCall { input, .. } = &resp.content[0] {
            assert_eq!(input["command"], "ls");
        }
    }

    #[test]
    fn parse_tool_finish_without_structured_call_is_error() {
        let body = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "call\n<invoke name=\"read_file\"></invoke>"
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 200,
                "completion_tokens": 30,
                "cost": 0.0123
            }
        });

        let response = crate::providers::validate_response_shape(parse_response(&body));

        assert_eq!(response.stop_reason, StopReason::Error);
        assert_eq!(
            response.error_message.as_deref(),
            Some("provider declared tool use but returned no structured tool call")
        );
        assert!(response.content.is_empty());
        assert!(!response.retryable);
        assert_eq!(response.usage.input, 200);
        assert!((response.usage.cost.total_usd - 0.0123).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_max_tokens_response() {
        let body = json!({
            "choices": [{"message": {"content": "partial"}, "finish_reason": "max_tokens"}],
            "usage": {}
        });
        assert_eq!(parse_response(&body).stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn parse_content_filter_is_refusal() {
        let body = json!({
            "choices": [{"message": {"content": "", "refusal": "policy restriction"}, "finish_reason": "content_filter"}],
            "usage": {}
        });
        let response = parse_response(&body);
        assert_eq!(response.stop_reason, StopReason::Refusal);
        assert_eq!(
            response.error_message.as_deref(),
            Some("policy restriction")
        );
    }

    #[test]
    fn parse_content_filter_without_reason_uses_safe_default() {
        let body = json!({
            "choices": [{"message": {"content": ""}, "finish_reason": "content_filter"}],
            "usage": {}
        });
        let response = parse_response(&body);
        assert_eq!(response.stop_reason, StopReason::Refusal);
        assert_eq!(
            response.error_message.as_deref(),
            Some("provider content policy refusal")
        );
    }

    #[test]
    fn parse_usage_maps_token_fields() {
        // Current OpenRouter/OpenAI format: cache counts are sub-details of
        // prompt_tokens (which INCLUDES them). Canonical semantics subtract
        // them out of `input` so prompt_tokens() == wire prompt_tokens.
        let body = json!({
            "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": 194,
                "completion_tokens": 50,
                "prompt_tokens_details": {"cached_tokens": 30, "cache_write_tokens": 20},
                "completion_tokens_details": {"reasoning_tokens": 0},
                "cost": 0.0015
            }
        });
        let resp = parse_response(&body);
        assert_eq!(
            resp.usage.input, 144,
            "input must be the NON-cached prompt tokens"
        );
        assert_eq!(resp.usage.output, 50);
        assert_eq!(resp.usage.reasoning_output, Some(0));
        assert_eq!(resp.usage.cache_read, 30);
        assert_eq!(resp.usage.cache_write, 20);
        assert!((resp.usage.cost.total_usd - 0.0015).abs() < f64::EPSILON);
        assert_eq!(
            resp.usage.prompt_tokens(),
            194,
            "invariant: prompt_tokens() == wire prompt_tokens"
        );
    }

    #[test]
    fn parse_usage_without_details_keeps_invariant() {
        // vLLM/Ollama/most self-hosted omit prompt_tokens_details (or send
        // null): input == prompt_tokens, caches 0, invariant still exact.
        let body = json!({
            "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 5, "prompt_tokens_details": null}
        });
        let resp = parse_response(&body);
        assert_eq!(resp.usage.input, 100);
        assert_eq!(resp.usage.cache_read, 0);
        assert_eq!(resp.usage.cache_write, 0);
        assert_eq!(resp.usage.reasoning_output, None);
        assert_eq!(resp.usage.prompt_tokens(), 100);
    }

    #[test]
    fn parse_usage_ignores_defunct_top_level_cache_fields() {
        // The old parser read these top-level names; the current wire format
        // doesn't have them and they must not be picked up if present.
        let body = json!({
            "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 5,
                "cache_read_input_tokens": 999,
                "cache_creation_input_tokens": 999
            }
        });
        let resp = parse_response(&body);
        assert_eq!(resp.usage.cache_read, 0);
        assert_eq!(resp.usage.cache_write, 0);
        assert_eq!(resp.usage.prompt_tokens(), 100);
    }

    // --- context-overflow classification (ADR-002, vikunja #964) ---

    #[test]
    fn overflow_classifier_matches_known_phrasings() {
        // One per documented upstream dialect.
        for msg in [
            "This model's maximum context length is 8192 tokens. However, your messages resulted in 9001 tokens.",
            "context length exceeded",
            "prompt is too long: 213462 tokens > 200000 maximum",
            "the request exceeds the available context size",
            "Input is too long for requested model",
        ] {
            assert!(is_context_overflow_error(msg), "should classify as overflow: {msg}");
        }
    }

    #[test]
    fn overflow_classifier_rejects_other_errors() {
        for msg in [
            "rate limit exceeded",
            "invalid api key",
            "model not found",
            "internal server error",
        ] {
            assert!(
                !is_context_overflow_error(msg),
                "must not classify as overflow: {msg}"
            );
        }
    }

    // --- context window (vikunja #965) ---

    #[test]
    fn context_length_found_by_id() {
        let body = json!({"data": [
            {"id": "other/model", "context_length": 8192},
            {"id": "anthropic/claude-sonnet-4.6", "context_length": 200000},
        ]});
        assert_eq!(
            context_length_from_models(&body, "anthropic/claude-sonnet-4.6"),
            Some(200_000)
        );
    }

    #[test]
    fn context_length_falls_back_to_max_model_len() {
        // Self-hosted vLLM: no context_length, exposes max_model_len instead.
        let body = json!({"data": [{"id": "local/llama", "max_model_len": 32768}]});
        assert_eq!(
            context_length_from_models(&body, "local/llama"),
            Some(32_768)
        );
    }

    #[test]
    fn context_length_prefers_context_length_over_max_model_len() {
        let body = json!({"data": [
            {"id": "m", "context_length": 128000, "max_model_len": 32768}
        ]});
        assert_eq!(context_length_from_models(&body, "m"), Some(128_000));
    }

    #[test]
    fn context_length_unknown_model_is_none() {
        let body = json!({"data": [{"id": "known", "context_length": 8192}]});
        assert_eq!(context_length_from_models(&body, "missing"), None);
    }

    #[test]
    fn context_length_missing_both_fields_is_none() {
        let body = json!({"data": [{"id": "m"}]});
        assert_eq!(context_length_from_models(&body, "m"), None);
    }

    #[test]
    fn context_length_zero_is_none() {
        let body = json!({"data": [{"id": "m", "context_length": 0}]});
        assert_eq!(context_length_from_models(&body, "m"), None);
    }

    #[test]
    fn context_length_no_data_array_is_none() {
        assert_eq!(context_length_from_models(&json!({}), "m"), None);
    }

    #[tokio::test]
    async fn context_window_returns_none_on_http_error() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let resp = b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
                stream.write_all(resp).await.ok();
            }
        });
        let provider =
            OpenRouterProvider::new("k".into(), format!("http://127.0.0.1:{port}")).unwrap();
        assert_eq!(provider.context_window("some/model").await, None);
    }

    #[test]
    fn parse_missing_usage_fields_default_to_zero() {
        let body = json!({
            "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
            "usage": {}
        });
        let resp = parse_response(&body);
        assert_eq!(resp.usage.input, 0);
        assert_eq!(resp.usage.output, 0);
        assert_eq!(resp.usage.cache_read, 0);
        assert_eq!(resp.usage.cache_write, 0);
        assert_eq!(resp.usage.cost.total_usd, 0.0);
    }

    // --- map_finish_reason ---

    #[test]
    fn finish_reason_all_variants() {
        assert_eq!(map_finish_reason(Some("stop")), StopReason::EndTurn);
        assert_eq!(map_finish_reason(Some("end_turn")), StopReason::EndTurn);
        assert_eq!(map_finish_reason(Some("tool_calls")), StopReason::ToolUse);
        assert_eq!(map_finish_reason(Some("max_tokens")), StopReason::MaxTokens);
        assert_eq!(
            map_finish_reason(Some("content_filter")),
            StopReason::Refusal
        );
        assert_eq!(map_finish_reason(Some("unknown_future")), StopReason::Error);
        assert_eq!(map_finish_reason(None), StopReason::Error);
    }

    // --- StreamState (vikunja #957) ---

    #[test]
    fn stream_text_deltas_accumulate_and_emit() {
        let mut state = StreamState::default();
        let e1 = state
            .on_chunk(&json!({"choices": [{"delta": {"content": "hel"}, "finish_reason": null}]}));
        let e2 = state
            .on_chunk(&json!({"choices": [{"delta": {"content": "lo"}, "finish_reason": null}]}));
        assert_eq!(e1, vec![StreamEvent::TextDelta("hel".to_string())]);
        assert_eq!(e2, vec![StreamEvent::TextDelta("lo".to_string())]);
        state.on_chunk(&json!({"choices": [{"delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 10, "completion_tokens": 2, "cost": 0.00042}}));
        let resp = state.finish();
        assert!(matches!(&resp.content[0], ContentBlock::Text(t) if t == "hello"));
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert_eq!(resp.usage.input, 10);
        assert_eq!(resp.usage.output, 2);
        assert!((resp.usage.cost.total_usd - 0.00042).abs() < f64::EPSILON);
    }

    #[test]
    fn stream_tool_finish_without_structured_call_is_error() {
        let mut state = StreamState::default();
        state.on_chunk(&json!({
            "choices": [{
                "delta": {"content": "call\n<invoke name=\"read_file\"></invoke>"},
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 200,
                "completion_tokens": 30,
                "cost": 0.0123
            }
        }));

        let response = crate::providers::validate_response_shape(state.finish());

        assert_eq!(response.stop_reason, StopReason::Error);
        assert_eq!(
            response.error_message.as_deref(),
            Some("provider declared tool use but returned no structured tool call")
        );
        assert!(response.content.is_empty());
        assert!(!response.retryable);
        assert_eq!(response.usage.input, 200);
        assert!((response.usage.cost.total_usd - 0.0123).abs() < f64::EPSILON);
    }

    #[test]
    fn stream_empty_content_delta_not_emitted() {
        let mut state = StreamState::default();
        let ev = state
            .on_chunk(&json!({"choices": [{"delta": {"content": ""}, "finish_reason": null}]}));
        assert!(ev.is_empty());
    }

    #[test]
    fn stream_tool_call_arguments_accumulate_silently() {
        let mut state = StreamState::default();
        let ev1 = state.on_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "exec", "arguments": ""}}]}, "finish_reason": null}]
        }));
        let ev2 = state.on_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{\"command\":"}}]}, "finish_reason": null}]
        }));
        let ev3 = state.on_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "\"ls\"}"}}]}, "finish_reason": "tool_calls"}]
        }));
        assert!(ev1.is_empty());
        assert!(ev2.is_empty());
        assert!(ev3.is_empty());
        let resp = state.finish();
        assert!(matches!(
            &resp.content[0],
            ContentBlock::ToolCall { id, name, input } if id == "call_1" && name == "exec" && input["command"] == "ls"
        ));
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn stream_multiple_tool_calls_tracked_by_index() {
        let mut state = StreamState::default();
        state.on_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call_a", "function": {"name": "read_file", "arguments": "{}"}},
                {"index": 1, "id": "call_b", "function": {"name": "exec", "arguments": "{}"}}
            ]}, "finish_reason": null}]
        }));
        state.on_chunk(&json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}));
        let resp = state.finish();
        assert_eq!(resp.content.len(), 2);
        assert!(
            matches!(&resp.content[0], ContentBlock::ToolCall { name, .. } if name == "read_file")
        );
        assert!(matches!(&resp.content[1], ContentBlock::ToolCall { name, .. } if name == "exec"));
    }

    #[test]
    fn stream_no_usage_chunk_defaults_to_zero() {
        let mut state = StreamState::default();
        state.on_chunk(&json!({"choices": [{"delta": {"content": "hi"}, "finish_reason": null}]}));
        state.on_chunk(&json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}));
        let resp = state.finish();
        assert_eq!(resp.usage.input, 0);
        assert_eq!(resp.usage.output, 0);
    }

    #[test]
    fn stream_usage_in_non_final_chunk_is_captured() {
        // xAI/Grok streaming puts usage in a NON-final chunk (an empty extra
        // chunk follows). Capture must be position-tolerant: last-seen
        // non-null usage wins, wherever it appears.
        let mut state = StreamState::default();
        state.on_chunk(&json!({"choices": [{"delta": {"content": "hi"}, "finish_reason": null}]}));
        state.on_chunk(&json!({
            "choices": [{"delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 42, "completion_tokens": 7}
        }));
        // Trailing chunk with null usage must NOT clobber the captured one.
        state.on_chunk(&json!({"choices": [], "usage": null}));
        let resp = state.finish();
        assert_eq!(resp.usage.prompt_tokens(), 42);
        assert_eq!(resp.usage.output, 7);
    }

    // --- reasoning (vikunja #1527) ---

    fn reasoning_ctx() -> Context {
        Context {
            system: None,
            messages: vec![Message::user("hi")],
            tools: vec![],
            stable_prefix_len: 0,
        }
    }

    fn reasoning_opts(thinking: ThinkingLevel) -> CompleteOpts {
        CompleteOpts {
            model: "anthropic/claude-opus-4.8".into(),
            thinking,
            ..CompleteOpts::default()
        }
    }

    #[test]
    fn request_maps_every_thinking_level_to_openrouter_reasoning() {
        let provider = OpenRouterProvider::new("k".into(), "http://unused".into()).unwrap();
        let cases = [
            (ThinkingLevel::Off, json!({"enabled": false})),
            (ThinkingLevel::Minimal, json!({"effort": "minimal"})),
            (ThinkingLevel::Low, json!({"effort": "low"})),
            (ThinkingLevel::Medium, json!({"effort": "medium"})),
            (ThinkingLevel::High, json!({"effort": "high"})),
            (ThinkingLevel::XHigh, json!({"effort": "xhigh"})),
            (ThinkingLevel::Max, json!({"effort": "max"})),
        ];
        for (level, expected) in cases {
            let body = provider.request_body(&reasoning_ctx(), &reasoning_opts(level), true);
            assert_eq!(body["reasoning"], expected);
        }
    }

    #[tokio::test]
    async fn catalog_fetch_supplies_model_output_ceiling_for_requests() {
        let catalog = json!({"data": [
            {"id": "anthropic/claude-opus-4.8", "created": 2, "context_length": 1_000_000,
             "top_provider": {"max_completion_tokens": 128_000}},
            {"id": "no/limit", "created": 1, "top_provider": {}}
        ]});
        let (base_url, _) =
            mock_http_server("200 OK", "application/json", catalog.to_string()).await;
        let provider = OpenRouterProvider::new("k".into(), base_url).unwrap();
        let before = provider.request_body(
            &reasoning_ctx(),
            &reasoning_opts(ThinkingLevel::High),
            false,
        );
        assert_eq!(before["max_tokens"], crate::providers::DEFAULT_MAX_TOKENS);

        assert!(provider.list_models().await.is_some());
        let after = provider.request_body(
            &reasoning_ctx(),
            &reasoning_opts(ThinkingLevel::High),
            false,
        );
        assert_eq!(after["max_tokens"], 128_000);
        assert_eq!(
            caps_from_models(&catalog)
                .get("no/limit")
                .and_then(|caps| caps.max_output),
            None,
            "models without a ceiling keep the generic default"
        );
    }

    #[test]
    fn catalog_reasoning_metadata_yields_only_honored_thinking_levels() {
        use ThinkingLevel::*;
        let entry = |reasoning: Value| json!({"id": "m", "reasoning": reasoning});
        let cases = [
            // anthropic/claude-opus-4.8: optional, no "none" effort.
            (
                entry(json!({"mandatory": false, "default_enabled": false,
                    "supported_efforts": ["max", "xhigh", "high", "medium", "low"]})),
                Some(vec![Off, Low, Medium, High, XHigh, Max]),
            ),
            // anthropic/claude-opus-5.5: mandatory reasoning cannot be turned off.
            (
                entry(json!({"mandatory": true,
                    "supported_efforts": ["max", "xhigh", "high", "medium", "low"]})),
                Some(vec![Low, Medium, High, XHigh, Max]),
            ),
            // openai/gpt-5.6-sol lists "none" explicitly.
            (
                entry(json!({"mandatory": false,
                    "supported_efforts": ["max", "xhigh", "high", "medium", "low", "none"]})),
                Some(vec![Off, Low, Medium, High, XHigh, Max]),
            ),
            // null = every gateway effort is accepted.
            (
                entry(json!({"mandatory": false, "supported_efforts": null})),
                Some(ThinkingLevel::ALL.to_vec()),
            ),
            // Omitted efforts = the model exposes no effort selection.
            (entry(json!({"mandatory": false})), None),
            (json!({"id": "m"}), None),
        ];
        for (model, expected) in cases {
            assert_eq!(thinking_levels_from_model(&model), expected, "{model}");
        }
    }

    #[tokio::test]
    async fn thinking_levels_come_from_the_cached_catalog() {
        let catalog = json!({"data": [{"id": "anthropic/claude-opus-5.5", "created": 1,
            "reasoning": {"mandatory": true, "supported_efforts": ["high", "low"]}}]});
        let (base_url, _) =
            mock_http_server("200 OK", "application/json", catalog.to_string()).await;
        let provider = OpenRouterProvider::new("k".into(), base_url).unwrap();
        assert!(provider.list_models().await.is_some());
        assert_eq!(
            provider.thinking_levels("anthropic/claude-opus-5.5").await,
            Some(vec![ThinkingLevel::Low, ThinkingLevel::High])
        );
        assert_eq!(provider.thinking_levels("not/listed").await, None);
    }

    #[test]
    fn parse_response_keeps_reasoning_text_and_replay_state() {
        let body = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "answer",
                    "reasoning": "thought",
                    "reasoning_details": [{
                        "type": "reasoning.text", "text": "thought",
                        "signature": "sig", "format": "anthropic-claude-v1", "index": 0
                    }],
                    "tool_calls": [{"id": "c1", "type": "function",
                        "function": {"name": "calc", "arguments": "{}"}}]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let resp = parse_response(&body);
        assert!(matches!(&resp.content[0], ContentBlock::Thinking(t) if t == "thought"));
        match &resp.content[1] {
            ContentBlock::ProviderState { provider, data } => {
                assert_eq!(provider, PROVIDER_STATE);
                assert_eq!(data[0]["signature"], "sig");
            }
            other => panic!("expected provider state, got {other:?}"),
        }
        assert!(matches!(&resp.content[2], ContentBlock::Text(t) if t == "answer"));
        assert!(matches!(&resp.content[3], ContentBlock::ToolCall { id, .. } if id == "c1"));
    }

    #[test]
    fn stream_merges_reasoning_fragments_and_emits_thinking_deltas() {
        // Shape observed from OpenRouter + anthropic/claude-opus-4.8: text
        // fragments share index 0 and the signature arrives alone, last.
        let detail = |fields: Value| {
            let mut detail =
                json!({"type": "reasoning.text", "format": "anthropic-claude-v1", "index": 0});
            detail
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            json!({"choices": [{"delta": {"reasoning_details": [detail]}}]})
        };
        let mut state = StreamState::default();
        let mut events = Vec::new();
        events.extend(state.on_chunk(&json!({"choices": [{"delta": {"reasoning": "Let "}}]})));
        events.extend(state.on_chunk(&detail(json!({"text": "Let "}))));
        events.extend(state.on_chunk(&json!({"choices": [{"delta": {"reasoning": "me"}}]})));
        events.extend(state.on_chunk(&detail(json!({"text": "me", "signature": null}))));
        events.extend(state.on_chunk(&detail(json!({"signature": "sig"}))));
        events.extend(state.on_chunk(
            &json!({"choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}]}),
        ));

        assert_eq!(
            events,
            vec![
                StreamEvent::ThinkingDelta("Let ".into()),
                StreamEvent::ThinkingDelta("me".into()),
                StreamEvent::TextDelta("done".into()),
            ]
        );
        let resp = state.finish();
        assert!(matches!(&resp.content[0], ContentBlock::Thinking(t) if t == "Let me"));
        match &resp.content[1] {
            ContentBlock::ProviderState { data, .. } => assert_eq!(
                data,
                &json!([{"type": "reasoning.text", "format": "anthropic-claude-v1",
                         "index": 0, "text": "Let me", "signature": "sig"}])
            ),
            other => panic!("expected provider state, got {other:?}"),
        }
    }

    #[test]
    fn stream_keeps_distinct_reasoning_detail_indices_separate() {
        let mut details = Vec::new();
        merge_reasoning_detail(
            &mut details,
            &json!({"type": "reasoning.summary", "summary": "a", "index": 0}),
        );
        merge_reasoning_detail(
            &mut details,
            &json!({"type": "reasoning.encrypted", "data": "x", "index": 1}),
        );
        merge_reasoning_detail(&mut details, &json!({"data": "y", "index": 1}));
        assert_eq!(details.len(), 2);
        assert_eq!(details[0]["summary"], "a");
        assert_eq!(details[1]["data"], "xy");
    }

    #[test]
    fn wire_replays_only_openrouter_reasoning_details() {
        let details =
            json!([{"type": "reasoning.text", "text": "t", "signature": "sig", "index": 0}]);
        let msgs = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking("t".into()),
                ContentBlock::ProviderState {
                    provider: PROVIDER_STATE.into(),
                    data: details.clone(),
                },
                ContentBlock::ProviderState {
                    provider: "openai".into(),
                    data: json!({"type": "reasoning", "encrypted_content": "opaque"}),
                },
                ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "calc".into(),
                    input: json!({}),
                },
            ],
        }];
        let wire = messages_to_wire(None, &msgs);
        assert_eq!(wire[0]["reasoning_details"], details);
        assert!(!wire[0].to_string().contains("opaque"));
    }
}
