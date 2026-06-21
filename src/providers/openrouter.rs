use async_trait::async_trait;
use serde_json::{json, Value};

use crate::config::AgentSettings;
use crate::providers::{
    CompleteOpts, ContentBlock, Context, Cost, LlmProvider, LlmResponse, Message, Role,
    StopReason, ToolSchema, Usage,
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
        Ok(Self { api_key, base_url, client })
    }

    pub fn from_config(settings: &AgentSettings) -> Result<Self, String> {
        let api_key = settings.resolve_api_key()?;
        Self::new(api_key, settings.base_url.clone())
    }
}

#[async_trait]
impl LlmProvider for OpenRouterProvider {
    async fn complete(&self, ctx: &Context, opts: &CompleteOpts) -> LlmResponse {
        let messages = messages_to_wire(ctx.system.as_deref(), &ctx.messages);
        let tools = tools_to_wire(&ctx.tools);

        let mut body = json!({
            "model": opts.model,
            "messages": messages,
            "max_tokens": opts.max_tokens,
            "stream": false,
        });

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
            return LlmResponse::error(format!("openrouter {status}: {msg}"));
        }

        parse_response(&resp_body)
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
                        if let ContentBlock::ToolResult { tool_use_id, content, .. } = b {
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
                } else {
                    let text: String = msg
                        .content
                        .iter()
                        .filter_map(|b| {
                            if let ContentBlock::Text(t) = b { Some(t.as_str()) } else { None }
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
                        // Thinking blocks are Anthropic-specific; skip for OpenRouter
                        ContentBlock::Thinking(_) => {}
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

    LlmResponse { content, stop_reason, error_message: None, usage }
}

pub(crate) fn map_finish_reason(reason: Option<&str>) -> StopReason {
    match reason {
        Some("stop") | Some("end_turn") => StopReason::EndTurn,
        Some("tool_calls") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        _ => StopReason::Error,
    }
}

fn parse_usage(usage: &Value) -> Usage {
    Usage {
        input: usage["prompt_tokens"].as_u64().unwrap_or(0),
        output: usage["completion_tokens"].as_u64().unwrap_or(0),
        cache_read: usage["cache_read_input_tokens"].as_u64().unwrap_or(0),
        cache_write: usage["cache_creation_input_tokens"].as_u64().unwrap_or(0),
        // OpenRouter does not return cost in the usage object; left at zero.
        // Fetch from /api/v1/generation?id={id} if per-run cost is needed.
        cost: Cost::default(),
    }
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
    fn parse_unknown_finish_reason_is_error() {
        let body = json!({
            "choices": [{"message": {"content": ""}, "finish_reason": "content_filter"}],
            "usage": {}
        });
        assert_eq!(parse_response(&body).stop_reason, StopReason::Error);
    }

    #[test]
    fn parse_usage_maps_token_fields() {
        let body = json!({
            "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "cache_read_input_tokens": 30,
                "cache_creation_input_tokens": 20
            }
        });
        let resp = parse_response(&body);
        assert_eq!(resp.usage.input, 100);
        assert_eq!(resp.usage.output, 50);
        assert_eq!(resp.usage.cache_read, 30);
        assert_eq!(resp.usage.cache_write, 20);
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
        assert_eq!(map_finish_reason(Some("content_filter")), StopReason::Error);
        assert_eq!(map_finish_reason(Some("unknown_future")), StopReason::Error);
        assert_eq!(map_finish_reason(None), StopReason::Error);
    }
}
