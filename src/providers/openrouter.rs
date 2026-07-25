use async_trait::async_trait;
use serde_json::{json, Value};

use crate::providers::{
    CompleteOpts, ContentBlock, Context, Cost, LlmProvider, LlmResponse, Message, Role, StopReason,
    StreamEvent, ToolSchema, Usage,
};

pub struct OpenRouterProvider {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
}

impl OpenRouterProvider {
    pub fn new(api_key: String, base_url: String) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| format!("build http client: {e}"))?;
        Ok(Self {
            api_key,
            base_url,
            client,
        })
    }
}

#[async_trait]
impl LlmProvider for OpenRouterProvider {
    fn supports_images(&self) -> bool {
        true
    }

    async fn complete(&self, ctx: &Context, opts: &CompleteOpts) -> LlmResponse {
        let messages = messages_to_wire(ctx.system.as_deref(), &ctx.messages);
        let tools = tools_to_wire(&ctx.tools);

        let mut body = json!({
            "model": opts.model,
            "messages": messages,
            "max_tokens": opts.max_tokens,
            "stream": false,
        });

        if let Some(t) = opts.temperature {
            body["temperature"] = json!(t);
        }
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let resp = match self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return LlmResponse::error(format!("openrouter request failed: {e}")),
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

        let messages = messages_to_wire(ctx.system.as_deref(), &ctx.messages);
        let tools = tools_to_wire(&ctx.tools);

        // No `stream_options`: OpenRouter deprecated `include_usage` (usage
        // is now always included in the final SSE chunk automatically).
        let mut body = json!({
            "model": opts.model,
            "messages": messages,
            "max_tokens": opts.max_tokens,
            "stream": true,
        });

        if let Some(t) = opts.temperature {
            body["temperature"] = json!(t);
        }
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let resp = match self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return LlmResponse::error(format!("openrouter request failed: {e}")),
        };

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            let full = format!("openrouter {status}: {body_text}");
            if is_context_overflow_error(&body_text) {
                return LlmResponse::context_overflow_error(full);
            }
            return LlmResponse::error(full);
        }

        let mut events = resp.bytes_stream().eventsource();
        let mut state = StreamState::default();

        while let Some(event) = events.next().await {
            let event = match event {
                Ok(e) => e,
                Err(e) => return LlmResponse::error(format!("openrouter stream error: {e}")),
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
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body: Value = resp.json().await.ok()?;
        context_length_from_models(&body, model)
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
        let mut content = Vec::new();
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
                        // Thinking/provider state are provider-specific; skip.
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
                wire.push(obj);
            }
        }
    }

    wire
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
                    "parameters": t.input_schema,
                }
            })
        })
        .collect()
}

/// Parse an OpenRouter (OpenAI-format) response body into our neutral types.
pub(crate) fn parse_response(body: &Value) -> LlmResponse {
    let choice = &body["choices"][0];
    let finish_reason = choice["finish_reason"].as_str();
    let stop_reason = map_finish_reason(finish_reason);

    let usage = parse_usage(&body["usage"]);
    let message = &choice["message"];

    let mut content: Vec<ContentBlock> = Vec::new();

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
        cache_read,
        cache_write,
        // OpenRouter does not return cost in the usage object; left at zero.
        // Fetch from /api/v1/generation?id={id} if per-run cost is needed.
        cost: Cost::default(),
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
    use serde_json::json;

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
                "prompt_tokens_details": {"cached_tokens": 30, "cache_write_tokens": 20}
            }
        });
        let resp = parse_response(&body);
        assert_eq!(
            resp.usage.input, 144,
            "input must be the NON-cached prompt tokens"
        );
        assert_eq!(resp.usage.output, 50);
        assert_eq!(resp.usage.cache_read, 30);
        assert_eq!(resp.usage.cache_write, 20);
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
        state.on_chunk(&json!({"choices": [{"delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 10, "completion_tokens": 2}}));
        let resp = state.finish();
        assert!(matches!(&resp.content[0], ContentBlock::Text(t) if t == "hello"));
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert_eq!(resp.usage.input, 10);
        assert_eq!(resp.usage.output, 2);
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
}
