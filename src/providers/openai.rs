use async_trait::async_trait;
use serde_json::{json, Value};

use crate::providers::{
    resolve_max_output, CompleteOpts, ContentBlock, Context, Cost, LlmProvider, LlmResponse,
    Message, Role, StopReason, StreamEvent, ThinkingLevel, ToolSchema, Usage,
};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const PROVIDER_STATE: &str = "openai";
const GPT_56_CONTEXT_WINDOW: u64 = 1_050_000;
const GPT_56_MAX_OUTPUT: u32 = 128_000;

pub struct OpenAiProvider {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
}

impl OpenAiProvider {
    pub fn new(api_key: String, base_url: String) -> Result<Self, String> {
        let base_url = if base_url.trim().is_empty() {
            DEFAULT_BASE_URL.to_string()
        } else {
            base_url
        };
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| format!("build http client: {e}"))?;
        Ok(Self {
            api_key,
            base_url,
            client,
        })
    }

    fn endpoint(&self) -> String {
        format!("{}/responses", self.base_url.trim_end_matches('/'))
    }

    fn request_body(&self, ctx: &Context, opts: &CompleteOpts, stream: bool) -> Value {
        build_request(ctx, opts, stream)
    }

    fn reject_images(ctx: &Context) -> Option<LlmResponse> {
        ctx.messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|block| matches!(block, ContentBlock::Image { .. }))
            .then(|| LlmResponse::error("native OpenAI provider does not support image input"))
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn complete(&self, ctx: &Context, opts: &CompleteOpts) -> LlmResponse {
        if let Some(error) = Self::reject_images(ctx) {
            return error;
        }
        let response = match self
            .client
            .post(self.endpoint())
            .bearer_auth(&self.api_key)
            .json(&self.request_body(ctx, opts, false))
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => return LlmResponse::error(format!("openai request failed: {error}")),
        };
        let status = response.status();
        let body: Value = match response.json().await {
            Ok(body) => body,
            Err(error) => return LlmResponse::error(format!("openai response parse: {error}")),
        };
        if !status.is_success() {
            return error_response(status.as_u16(), &body);
        }
        parse_response(&body, &opts.model)
    }

    async fn stream(
        &self,
        ctx: &Context,
        opts: &CompleteOpts,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> LlmResponse {
        use eventsource_stream::Eventsource;
        use futures_util::StreamExt;

        if let Some(error) = Self::reject_images(ctx) {
            return error;
        }
        let response = match self
            .client
            .post(self.endpoint())
            .bearer_auth(&self.api_key)
            .json(&self.request_body(ctx, opts, true))
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => return LlmResponse::error(format!("openai request failed: {error}")),
        };
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            let body =
                serde_json::from_str(&text).unwrap_or_else(|_| json!({"error":{"message":text}}));
            return error_response(status.as_u16(), &body);
        }

        let mut stream = response.bytes_stream().eventsource();
        let mut state = StreamState::new(opts.model.clone());
        while let Some(event) = stream.next().await {
            let event = match event {
                Ok(event) => event,
                Err(error) => return LlmResponse::error(format!("openai stream error: {error}")),
            };
            if event.data == "[DONE]" {
                break;
            }
            let payload: Value = match serde_json::from_str(&event.data) {
                Ok(payload) => payload,
                Err(_) => continue,
            };
            for delta in state.on_event(&payload) {
                on_event(delta);
            }
            if state.finished {
                break;
            }
        }
        state.finish()
    }

    async fn context_window(&self, model: &str) -> Option<u64> {
        known_context_window(model)
    }
}

fn is_gpt_56_sol(model: &str) -> bool {
    // Official model docs specify that the bare `gpt-5.6` alias routes to
    // GPT-5.6 Sol, so it shares the same limits and pricing.
    model == "gpt-5.6" || model == "gpt-5.6-sol" || model.starts_with("gpt-5.6-sol-")
}

pub(crate) fn known_context_window(model: &str) -> Option<u64> {
    is_gpt_56_sol(model).then_some(GPT_56_CONTEXT_WINDOW)
}

pub(crate) fn build_request(ctx: &Context, opts: &CompleteOpts, stream: bool) -> Value {
    // A fresh session inherits CompleteOpts' 8192 sentinel; with reasoning on
    // that truncates before the answer (incomplete_details.reason =
    // "max_output_tokens"). Substitute the model's real max output for the
    // sentinel, honor explicit values, and never exceed the model ceiling.
    let max_output_tokens = if is_gpt_56_sol(&opts.model) {
        resolve_max_output(
            opts.max_tokens,
            Some(GPT_56_MAX_OUTPUT),
            Some(GPT_56_MAX_OUTPUT),
        )
    } else {
        resolve_max_output(opts.max_tokens, None, None)
    };
    let mut body = json!({
        "model": opts.model,
        "input": messages_to_input(&ctx.messages),
        "max_output_tokens": max_output_tokens,
        "stream": stream,
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "reasoning": {
            "effort": reasoning_effort(&opts.thinking)
        }
    });
    if opts.thinking != ThinkingLevel::Off {
        body["reasoning"]["summary"] = json!("auto");
    }
    if let Some(system) = &ctx.system {
        body["instructions"] = json!(system);
    }
    let tools = tools_to_wire(&ctx.tools);
    if !tools.is_empty() {
        body["tools"] = json!(tools);
        body["tool_choice"] = json!("auto");
        body["parallel_tool_calls"] = json!(true);
    }
    // GPT-5 reasoning models use reasoning.effort and may reject sampling
    // parameters. Intentionally omit temperature, including for summarization.
    body
}

fn reasoning_effort(level: &ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "none",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
        // OpenAI's broadly deployed Responses contract tops out at xhigh;
        // map Daimonos's provider-neutral Max conservatively for compatibility.
        ThinkingLevel::Max => "xhigh",
    }
}

pub(crate) fn messages_to_input(messages: &[Message]) -> Vec<Value> {
    fn flush_text(input: &mut Vec<Value>, role: &str, text: &mut Vec<Value>) {
        if !text.is_empty() {
            input.push(json!({
                "type": "message",
                "role": role,
                "content": std::mem::take(text)
            }));
        }
    }

    let mut input = Vec::new();
    for message in messages {
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let mut text = Vec::new();
        for block in &message.content {
            match block {
                ContentBlock::Text(value) => text.push(if message.role == Role::User {
                    json!({"type": "input_text", "text": value})
                } else {
                    json!({"type": "output_text", "text": value})
                }),
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if message.role == Role::User => {
                    flush_text(&mut input, role, &mut text);
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": tool_use_id,
                        "output": content
                    }));
                }
                ContentBlock::ToolCall {
                    id,
                    name,
                    input: arguments,
                } if message.role == Role::Assistant => {
                    flush_text(&mut input, role, &mut text);
                    input.push(json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": arguments.to_string()
                    }));
                }
                ContentBlock::ProviderState { provider, data }
                    if message.role == Role::Assistant && provider == PROVIDER_STATE =>
                {
                    flush_text(&mut input, role, &mut text);
                    if let Some(reasoning) = sanitize_reasoning_state(data) {
                        input.push(reasoning);
                    } else {
                        tracing::warn!(
                            target: "daimonos::providers::openai",
                            event = "invalid_provider_state_skipped",
                        );
                    }
                }
                ContentBlock::ProviderState { provider, .. } => tracing::debug!(
                    target: "daimonos::providers::openai",
                    event = "provider_state_skipped",
                    owner = provider,
                    role,
                ),
                // Blocks in the wrong role are invalid neutral history and are
                // deliberately ignored rather than sent on an invalid wire.
                _ => {}
            }
        }
        flush_text(&mut input, role, &mut text);
    }
    input
}

fn sanitize_reasoning_state(data: &Value) -> Option<Value> {
    if data["type"].as_str() != Some("reasoning") {
        return None;
    }
    let encrypted = data["encrypted_content"].as_str()?;
    let summary = data["summary"].as_array().cloned().unwrap_or_default();
    let mut state = json!({
        "type": "reasoning",
        "summary": summary,
        "encrypted_content": encrypted,
    });
    if let Some(id) = data["id"].as_str() {
        state["id"] = json!(id);
    }
    if let Some(content) = data["content"].as_array() {
        state["content"] = json!(content);
    }
    Some(state)
}

pub(crate) fn tools_to_wire(tools: &[ToolSchema]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
                "strict": false
            })
        })
        .collect()
}

pub(crate) fn parse_response(body: &Value, model: &str) -> LlmResponse {
    let status = body["status"].as_str().unwrap_or_else(|| {
        if body["output"].is_array() && body["error"].is_null() {
            "completed"
        } else {
            "failed"
        }
    });
    let content = output_to_content(body["output"].as_array());
    let has_tools = content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolCall { .. }));
    let refusal = refusal_from_output(body["output"].as_array());
    let stop_reason = match status {
        "completed" if refusal.is_some() => StopReason::Refusal,
        "completed" if has_tools => StopReason::ToolUse,
        "completed" => StopReason::EndTurn,
        "incomplete"
            if body["incomplete_details"]["reason"].as_str() == Some("max_output_tokens") =>
        {
            StopReason::MaxTokens
        }
        _ => StopReason::Error,
    };
    let error_message = match stop_reason {
        StopReason::Refusal => refusal,
        StopReason::Error => Some(response_error_message(body)),
        _ => None,
    };
    let context_overflow = error_message
        .as_deref()
        .is_some_and(is_context_overflow_error);
    LlmResponse {
        content,
        stop_reason,
        error_message,
        context_overflow,
        usage: parse_usage(&body["usage"], model),
    }
}

fn refusal_from_output(output: Option<&Vec<Value>>) -> Option<String> {
    output.into_iter().flatten().find_map(|item| {
        (item["type"].as_str() == Some("message")).then_some(())?;
        item["content"].as_array()?.iter().find_map(|part| {
            (part["type"].as_str() == Some("refusal")).then(|| {
                part["refusal"]
                    .as_str()
                    .unwrap_or("provider refusal")
                    .to_string()
            })
        })
    })
}

fn output_to_content(output: Option<&Vec<Value>>) -> Vec<ContentBlock> {
    let mut content = Vec::new();
    for item in output.into_iter().flatten() {
        match item["type"].as_str() {
            Some("message") => {
                for part in item["content"].as_array().into_iter().flatten() {
                    match part["type"].as_str() {
                        Some("output_text") => {
                            if let Some(text) = part["text"].as_str().filter(|t| !t.is_empty()) {
                                content.push(ContentBlock::Text(text.to_string()));
                            }
                        }
                        // Refusal text is returned separately as error_message;
                        // never encode it into visible text with a sentinel.
                        Some("refusal") => {}
                        _ => {}
                    }
                }
            }
            Some("function_call") => {
                let call_id = item["call_id"].as_str().unwrap_or("");
                let tool_name = item["name"].as_str().unwrap_or("");
                let Some(args) = item["arguments"].as_str() else {
                    tracing::warn!(
                        target: "daimonos::providers::openai",
                        event = "invalid_function_call_arguments_skipped",
                        call_id,
                        tool_name,
                        error = "missing arguments string",
                    );
                    continue;
                };
                let input = match serde_json::from_str(args) {
                    Ok(input) => input,
                    Err(error) => {
                        tracing::warn!(
                            target: "daimonos::providers::openai",
                            event = "invalid_function_call_arguments_skipped",
                            call_id,
                            tool_name,
                            %error,
                        );
                        continue;
                    }
                };
                content.push(ContentBlock::ToolCall {
                    id: call_id.to_string(),
                    name: tool_name.to_string(),
                    input,
                });
            }
            Some("reasoning") => {
                // Whitelist the fields accepted by Responses input items;
                // discard response-local status/metadata before replay.
                if let Some(encrypted) = item["encrypted_content"].as_str() {
                    let mut state = json!({
                        "type": "reasoning",
                        "summary": item["summary"].as_array().cloned().unwrap_or_default(),
                        "encrypted_content": encrypted
                    });
                    if let Some(id) = item.get("id").filter(|value| !value.is_null()) {
                        state["id"] = id.clone();
                    }
                    if let Some(reasoning_content) =
                        item.get("content").filter(|value| !value.is_null())
                    {
                        state["content"] = reasoning_content.clone();
                    }
                    content.push(ContentBlock::ProviderState {
                        provider: PROVIDER_STATE.to_string(),
                        data: state,
                    });
                }
                for summary in item["summary"].as_array().into_iter().flatten() {
                    if let Some(text) = summary["text"].as_str().filter(|t| !t.is_empty()) {
                        content.push(ContentBlock::Thinking(text.to_string()));
                    }
                }
            }
            _ => {}
        }
    }
    content
}

fn parse_usage(usage: &Value, model: &str) -> Usage {
    let total_input = usage["input_tokens"].as_u64().unwrap_or(0);
    let cache_read = usage["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0);
    let output = usage["output_tokens"].as_u64().unwrap_or(0);
    let reasoning_output = usage["output_tokens_details"]["reasoning_tokens"]
        .as_u64()
        .unwrap_or(0);
    let input = total_input.saturating_sub(cache_read);
    let (input_rate, cache_rate, output_rate) = pricing(model);
    if (input > 0 || cache_read > 0 || output > 0)
        && input_rate == 0.0
        && cache_rate == 0.0
        && output_rate == 0.0
    {
        tracing::debug!(
            target: "daimonos::providers::openai",
            event = "unknown_model_pricing",
            model,
        );
    }
    let input_usd = input as f64 / 1_000_000.0 * input_rate;
    let cache_read_usd = cache_read as f64 / 1_000_000.0 * cache_rate;
    let output_usd = output as f64 / 1_000_000.0 * output_rate;
    Usage {
        input,
        output,
        reasoning_output,
        cache_read,
        cache_write: 0,
        cost: Cost {
            input_usd,
            output_usd,
            cache_read_usd,
            cache_write_usd: 0.0,
            total_usd: input_usd + cache_read_usd + output_usd,
        },
    }
}

fn pricing(model: &str) -> (f64, f64, f64) {
    if is_gpt_56_sol(model) {
        (5.0, 0.5, 30.0)
    } else {
        (0.0, 0.0, 0.0)
    }
}

fn response_error_message(body: &Value) -> String {
    let message = body["error"]["message"]
        .as_str()
        .or_else(|| body["incomplete_details"]["reason"].as_str())
        .unwrap_or("unknown error");
    bounded(message, 500)
}

fn error_response(status: u16, body: &Value) -> LlmResponse {
    let message = body["error"]["message"].as_str().unwrap_or("unknown error");
    let full = format!("openai {status}: {}", bounded(message, 500));
    if is_context_overflow_error(message) {
        LlmResponse::context_overflow_error(full)
    } else {
        LlmResponse::error(full)
    }
}

fn bounded(value: &str, cap: usize) -> String {
    value.chars().take(cap).collect()
}

pub(crate) fn is_context_overflow_error(message: &str) -> bool {
    let value = message.to_ascii_lowercase().replace(['_', '-'], " ");
    value.contains("maximum context length")
        || value.contains("context length exceeded")
        || value.contains("context window exceeded")
        || value.contains("exceeds the context window")
        || value.contains("prompt is too long")
        || value.contains("input is too long")
}

fn stream_error_message(event: &Value) -> String {
    for value in [
        &event["message"],
        &event["error"]["message"],
        &event["response"]["error"]["message"],
    ] {
        if let Some(message) = value.as_str() {
            return message.to_string();
        }
    }
    for value in [
        &event["code"],
        &event["error"]["code"],
        &event["response"]["error"]["code"],
    ] {
        if !value.is_null() {
            return format!("provider error code {value}");
        }
    }
    "unknown error".to_string()
}

struct StreamState {
    model: String,
    content: Vec<ContentBlock>,
    text: String,
    thinking: String,
    terminal: Option<LlmResponse>,
    finished: bool,
}

impl StreamState {
    fn new(model: String) -> Self {
        Self {
            model,
            content: Vec::new(),
            text: String::new(),
            thinking: String::new(),
            terminal: None,
            finished: false,
        }
    }

    fn on_event(&mut self, event: &Value) -> Vec<StreamEvent> {
        let mut deltas = Vec::new();
        match event["type"].as_str() {
            Some("response.output_text.delta") => {
                if let Some(delta) = event["delta"].as_str() {
                    self.text.push_str(delta);
                    deltas.push(StreamEvent::TextDelta(delta.to_string()));
                }
            }
            Some("response.reasoning_summary_text.delta") => {
                if let Some(delta) = event["delta"].as_str() {
                    self.thinking.push_str(delta);
                    deltas.push(StreamEvent::ThinkingDelta(delta.to_string()));
                }
            }
            Some("response.output_item.done") => {
                self.content
                    .extend(output_to_content(Some(&vec![event["item"].clone()])));
            }
            Some("response.completed") | Some("response.incomplete") | Some("response.failed") => {
                let mut parsed = parse_response(&event["response"], &self.model);
                // Terminal response is authoritative even when output is empty;
                // replace accumulated output-item events to avoid stale items.
                self.content = std::mem::take(&mut parsed.content);
                if !self.text.is_empty()
                    && !self
                        .content
                        .iter()
                        .any(|b| matches!(b, ContentBlock::Text(_)))
                {
                    self.content
                        .insert(0, ContentBlock::Text(std::mem::take(&mut self.text)));
                }
                if !self.thinking.is_empty()
                    && !self
                        .content
                        .iter()
                        .any(|b| matches!(b, ContentBlock::Thinking(_)))
                {
                    self.content
                        .push(ContentBlock::Thinking(std::mem::take(&mut self.thinking)));
                }
                parsed.content = std::mem::take(&mut self.content);
                self.terminal = Some(parsed);
                self.finished = true;
            }
            Some("error") | Some("response.error") => {
                let message = stream_error_message(event);
                let full = format!("openai stream error: {}", bounded(&message, 500));
                self.terminal = Some(if is_context_overflow_error(&message) {
                    LlmResponse::context_overflow_error(full)
                } else {
                    LlmResponse::error(full)
                });
                self.finished = true;
            }
            _ => {}
        }
        deltas
    }

    fn finish(mut self) -> LlmResponse {
        if let Some(response) = self.terminal.take() {
            return response;
        }
        LlmResponse::error("openai stream ended before terminal response")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn mock_server(
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
                                .and_then(|v| v.parse::<usize>().ok())
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

    fn opts() -> CompleteOpts {
        CompleteOpts {
            model: "gpt-5.6-sol".into(),
            max_tokens: 4096,
            thinking: ThinkingLevel::High,
            temperature: None,
        }
    }

    #[tokio::test]
    async fn complete_posts_authenticated_responses_request() {
        let response = json!({
            "status":"completed",
            "output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]}],
            "usage":{"input_tokens":12,"input_tokens_details":{"cached_tokens":2},"output_tokens":3}
        })
        .to_string();
        let (base_url, captured) = mock_server("200 OK", "application/json", response).await;
        let provider = OpenAiProvider::new("secret-key".into(), base_url).unwrap();
        let ctx = Context {
            messages: vec![Message::user("hello")],
            system: Some("sys".into()),
            tools: vec![],
            stable_prefix_len: 0,
        };
        let result = provider.complete(&ctx, &opts()).await;
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert_eq!(result.usage.input, 10);
        let request = captured.await.unwrap();
        assert!(request.starts_with("POST /responses HTTP/1.1"));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer secret-key"));
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["model"], "gpt-5.6-sol");
        assert_eq!(body["stream"], false);
        assert_eq!(body["instructions"], "sys");
    }

    #[tokio::test]
    async fn stream_consumes_responses_sse_and_forwards_deltas() {
        let terminal = json!({
            "type":"response.completed",
            "response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"hello"}]}],"usage":{"input_tokens":10,"output_tokens":2}}
        });
        let sse = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"type":"response.output_text.delta","delta":"hello"}),
            terminal
        );
        let (base_url, _) = mock_server("200 OK", "text/event-stream", sse).await;
        let provider = OpenAiProvider::new("key".into(), base_url).unwrap();
        let ctx = Context {
            messages: vec![Message::user("hi")],
            system: None,
            tools: vec![],
            stable_prefix_len: 0,
        };
        let mut events = Vec::new();
        let result = provider
            .stream(&ctx, &opts(), &mut |event| events.push(event))
            .await;
        assert_eq!(events, vec![StreamEvent::TextDelta("hello".into())]);
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert_eq!(result.usage.output, 2);
    }

    #[tokio::test]
    async fn non_success_response_is_bounded_and_classifies_overflow() {
        let body = json!({"error":{"message":"maximum context length exceeded"}}).to_string();
        let (base_url, _) = mock_server("400 Bad Request", "application/json", body).await;
        let provider = OpenAiProvider::new("key".into(), base_url).unwrap();
        let ctx = Context {
            messages: vec![],
            system: None,
            tools: vec![],
            stable_prefix_len: 0,
        };
        let result = provider.complete(&ctx, &opts()).await;
        assert_eq!(result.stop_reason, StopReason::Error);
        assert!(result.context_overflow);
    }

    #[test]
    fn request_serializes_responses_tools_and_history() {
        let ctx = Context {
            system: Some("system".into()),
            messages: vec![
                Message::user("inspect"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::ProviderState {
                            provider: "openai".into(),
                            data: json!({"type":"reasoning","id":"r1","encrypted_content":"opaque","summary":[]}),
                        },
                        ContentBlock::ToolCall {
                            id: "c1".into(),
                            name: "read_file".into(),
                            input: json!({"path":"a"}),
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
            tools: vec![ToolSchema {
                name: "read_file".into(),
                description: "read".into(),
                input_schema: json!({"type":"object"}),
            }],
            stable_prefix_len: 0,
        };
        let body = build_request(&ctx, &opts(), true);
        assert_eq!(body["instructions"], "system");
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert!(body["tools"][0].get("function").is_none());
        let input = body["input"].as_array().unwrap();
        assert_eq!(
            input
                .iter()
                .map(|item| item["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "message",
                "reasoning",
                "function_call",
                "function_call_output"
            ]
        );
        assert!(input
            .iter()
            .any(|v| v["type"] == "reasoning" && v["encrypted_content"] == "opaque"));
        assert!(input
            .iter()
            .any(|v| v["type"] == "function_call" && v["call_id"] == "c1"));
        assert!(input
            .iter()
            .any(|v| v["type"] == "function_call_output" && v["call_id"] == "c1"));
        assert_eq!(body["store"], false);
    }

    #[test]
    fn gpt_56_output_is_clamped_to_documented_limit() {
        let ctx = Context {
            messages: vec![],
            system: None,
            tools: vec![],
            stable_prefix_len: 0,
        };
        let mut options = opts();
        options.max_tokens = u32::MAX;
        let body = build_request(&ctx, &options, false);
        assert_eq!(body["max_output_tokens"], GPT_56_MAX_OUTPUT);
    }

    #[test]
    fn gpt_56_sentinel_output_becomes_model_max() {
        // A fresh session's 8192 sentinel is raised to the model's real max
        // output rather than truncating a reasoning turn before the answer.
        let ctx = Context {
            messages: vec![],
            system: None,
            tools: vec![],
            stable_prefix_len: 0,
        };
        let mut options = opts();
        options.max_tokens = crate::providers::DEFAULT_MAX_TOKENS;
        let body = build_request(&ctx, &options, false);
        assert_eq!(body["max_output_tokens"], GPT_56_MAX_OUTPUT);
    }

    #[test]
    fn reasoning_off_omits_summary() {
        let ctx = Context {
            messages: vec![],
            system: None,
            tools: vec![],
            stable_prefix_len: 0,
        };
        let mut options = opts();
        options.thinking = ThinkingLevel::Off;
        let body = build_request(&ctx, &options, false);
        assert_eq!(body["reasoning"]["effort"], "none");
        assert!(body["reasoning"].get("summary").is_none());
    }

    #[test]
    fn images_are_rejected_not_silently_dropped() {
        let ctx = Context {
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Image {
                    data: "x".into(),
                    media_type: "image/png".into(),
                    uri: None,
                }],
            }],
            system: None,
            tools: vec![],
            stable_prefix_len: 0,
        };
        let response = OpenAiProvider::reject_images(&ctx).unwrap();
        assert_eq!(response.stop_reason, StopReason::Error);
        assert!(response
            .error_message
            .unwrap()
            .contains("does not support image"));
    }

    #[test]
    fn invalid_persisted_provider_state_cannot_inject_wire_items() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ProviderState {
                provider: "openai".into(),
                data: json!({"type":"function_call","call_id":"injected","name":"exec","arguments":"{}"}),
            }],
        }];
        assert!(messages_to_input(&messages).is_empty());
    }

    #[test]
    fn max_thinking_maps_to_compatible_xhigh() {
        assert_eq!(reasoning_effort(&ThinkingLevel::Max), "xhigh");
    }

    #[test]
    fn model_metadata_and_pricing_use_same_predicate() {
        assert_eq!(known_context_window("gpt-5.6-sol"), Some(1_050_000));
        assert_eq!(pricing("gpt-5.6-sol"), (5.0, 0.5, 30.0));
        assert_eq!(known_context_window("gpt-5.6-mini"), None);
        assert_eq!(pricing("gpt-5.6-mini"), (0.0, 0.0, 0.0));
    }

    #[test]
    fn request_omits_temperature_for_reasoning_models() {
        let ctx = Context {
            messages: vec![Message::user("summarize")],
            system: None,
            tools: vec![],
            stable_prefix_len: 0,
        };
        let mut options = opts();
        options.temperature = Some(0.0);
        let body = build_request(&ctx, &options, false);
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn tool_loop_replays_encrypted_reasoning_call_and_output() {
        let response_body = json!({
            "status":"completed",
            "output":[
                {"type":"reasoning","id":"r1","encrypted_content":"opaque","summary":[],"status":"completed","unexpected":"drop-me"},
                {"type":"function_call","id":"fc1","call_id":"c1","name":"read_file","arguments":"{\"path\":\"a\"}"}
            ],
            "usage":{}
        });
        let first = parse_response(&response_body, "gpt-5.6-sol");
        assert_eq!(first.stop_reason, StopReason::ToolUse);
        let input = messages_to_input(&[
            Message::user("inspect"),
            Message {
                role: Role::Assistant,
                content: first.content,
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "c1".into(),
                    content: "file body".into(),
                    is_error: false,
                }],
            },
        ]);
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[1]["encrypted_content"], "opaque");
        assert!(input[1].get("status").is_none());
        assert!(input[1].get("unexpected").is_none());
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "c1");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "c1");
        assert_eq!(input[3]["output"], "file body");
    }

    #[test]
    fn visible_text_with_refusal_prefix_is_not_misclassified() {
        let response = parse_response(
            &json!({
                "status":"completed",
                "output":[{"type":"message","content":[{"type":"output_text","text":"[REFUSAL] is a literal marker"}]}],
                "usage":{}
            }),
            "gpt-5.6-sol",
        );
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        assert!(
            matches!(&response.content[0], ContentBlock::Text(text) if text.starts_with("[REFUSAL]"))
        );
    }

    #[test]
    fn multiple_function_calls_preserve_output_order() {
        let body = json!({
            "status":"completed",
            "output":[
                {"type":"function_call","call_id":"c1","name":"read_file","arguments":"{\"path\":\"a\"}"},
                {"type":"function_call","call_id":"c2","name":"search","arguments":"{\"pattern\":\"x\"}"}
            ],
            "usage":{}
        });
        let response = parse_response(&body, "gpt-5.6-sol");
        let ids = response
            .content
            .iter()
            .filter_map(|block| {
                if let ContentBlock::ToolCall { id, .. } = block {
                    Some(id.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["c1", "c2"]);
    }

    #[test]
    fn missing_status_with_output_is_treated_as_completed() {
        let response = parse_response(
            &json!({
                "output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]}],
                "usage":{}
            }),
            "gpt-5.6-sol",
        );
        assert_eq!(response.stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn reasoning_without_summary_replays_with_empty_summary() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ProviderState {
                provider: "openai".into(),
                data: json!({"type":"reasoning","encrypted_content":"opaque"}),
            }],
        }];
        let input = messages_to_input(&messages);
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["summary"], json!([]));
    }

    #[test]
    fn reasoning_without_encrypted_content_is_not_persisted() {
        let response = parse_response(
            &json!({
                "status":"completed",
                "output":[{"type":"reasoning","summary":[{"type":"summary_text","text":"visible summary"}]}],
                "usage":{}
            }),
            "gpt-5.6-sol",
        );
        assert!(!response
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ProviderState { .. })));
        assert!(response.content.iter().any(
            |block| matches!(block, ContentBlock::Thinking(text) if text == "visible summary")
        ));
    }

    #[test]
    fn response_parses_text_reasoning_tools_and_usage_without_double_counting() {
        let body = json!({
            "status":"completed",
            "output":[
                {"type":"reasoning","id":"r1","encrypted_content":"enc","summary":[{"type":"summary_text","text":"thought"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello","annotations":[]}]},
                {"type":"function_call","call_id":"c1","name":"exec","arguments":"{\"command\":\"true\"}"}
            ],
            "usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":40},"output_tokens":30,"output_tokens_details":{"reasoning_tokens":20}}
        });
        let response = parse_response(&body, "gpt-5.6-sol");
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(response.usage.input, 60);
        assert_eq!(response.usage.cache_read, 40);
        assert_eq!(response.usage.output, 30);
        assert_eq!(response.usage.reasoning_output, 20);
        assert!(response.content.iter().any(
            |b| matches!(b, ContentBlock::ProviderState { provider, .. } if provider == "openai")
        ));
        assert!(response
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Thinking(t) if t == "thought")));
        assert!(response.content.iter().any(
            |b| matches!(b, ContentBlock::ToolCall { id, name, .. } if id == "c1" && name == "exec")
        ));
    }

    #[test]
    fn raw_reasoning_text_is_not_exposed() {
        let mut state = StreamState::new("gpt-5.6-sol".into());
        let events = state.on_event(&json!({
            "type":"response.reasoning_text.delta",
            "delta":"private chain of thought"
        }));
        assert!(events.is_empty());
        assert!(state.thinking.is_empty());
    }

    #[test]
    fn numeric_stream_error_code_is_preserved() {
        let event = json!({"type":"response.error","error":{"code":429}});
        assert_eq!(stream_error_message(&event), "provider error code 429");
    }

    #[test]
    fn stream_error_classifies_context_overflow() {
        let mut state = StreamState::new("gpt-5.6-sol".into());
        state.on_event(&json!({
            "type":"response.error",
            "error":{"message":"context_length_exceeded"}
        }));
        let response = state.finish();
        assert_eq!(response.stop_reason, StopReason::Error);
        assert!(response.context_overflow);
    }

    #[test]
    fn stream_preserves_reasoning_delta_when_terminal_omits_summary() {
        let mut state = StreamState::new("gpt-5.6-sol".into());
        state.on_event(&json!({"type":"response.reasoning_summary_text.delta","delta":"why"}));
        state.on_event(&json!({
            "type":"response.completed",
            "response":{"status":"completed","output":[],"usage":{}}
        }));
        let response = state.finish();
        assert!(response
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Thinking(text) if text == "why")));
    }

    #[test]
    fn terminal_empty_output_drops_stale_completed_items() {
        let mut state = StreamState::new("gpt-5.6-sol".into());
        state.on_event(&json!({
            "type":"response.output_item.done",
            "item":{"type":"function_call","call_id":"stale","name":"exec","arguments":"{}"}
        }));
        state.on_event(&json!({
            "type":"response.completed",
            "response":{"status":"completed","output":[],"usage":{}}
        }));
        let response = state.finish();
        assert!(!response
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall { .. })));
    }

    #[test]
    fn stream_state_emits_deltas_and_finishes_from_completed_response() {
        let mut state = StreamState::new("gpt-5.6-sol".into());
        assert_eq!(
            state.on_event(&json!({"type":"response.output_text.delta","delta":"hi"})),
            vec![StreamEvent::TextDelta("hi".into())]
        );
        assert_eq!(
            state.on_event(&json!({"type":"response.reasoning_summary_text.delta","delta":"why"})),
            vec![StreamEvent::ThinkingDelta("why".into())]
        );
        state.on_event(&json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"hi"}]}],"usage":{"input_tokens":10,"output_tokens":2}}}));
        let response = state.finish();
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        assert!(matches!(&response.content[0], ContentBlock::Text(t) if t == "hi"));
    }

    #[test]
    fn incomplete_response_drops_truncated_function_call_arguments() {
        let response = parse_response(
            &json!({
                "status":"incomplete",
                "incomplete_details":{"reason":"max_output_tokens"},
                "output":[
                    {"type":"function_call","call_id":"complete","name":"read_file","arguments":"{\"path\":\"a\"}"},
                    {"type":"function_call","call_id":"truncated","name":"edit_file","arguments":"{\"path\":\"src/lib.rs\",\"edits\":["}
                ],
                "usage":{}
            }),
            "gpt-5.6-sol",
        );

        assert_eq!(response.stop_reason, StopReason::MaxTokens);
        let tool_call_ids = response
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(tool_call_ids, vec!["complete"]);
    }

    #[test]
    fn incomplete_refusal_and_errors_map_safely() {
        let incomplete = parse_response(
            &json!({"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[],"usage":{}}),
            "gpt-5.6-sol",
        );
        assert_eq!(incomplete.stop_reason, StopReason::MaxTokens);
        let refusal = parse_response(
            &json!({"status":"completed","output":[{"type":"message","content":[{"type":"refusal","refusal":"no"}]}],"usage":{}}),
            "gpt-5.6-sol",
        );
        assert_eq!(refusal.stop_reason, StopReason::Refusal);
        assert_eq!(refusal.error_message.as_deref(), Some("no"));
        assert!(is_context_overflow_error("Maximum context length exceeded"));
        let failed = parse_response(
            &json!({
                "status":"failed",
                "error":{"message":"maximum context length exceeded"},
                "output":[],
                "usage":{}
            }),
            "gpt-5.6-sol",
        );
        assert!(failed.context_overflow);
    }

    #[tokio::test]
    async fn known_context_window_is_available_without_network() {
        let provider = OpenAiProvider::new("key".into(), "".into()).unwrap();
        assert_eq!(
            provider.context_window("gpt-5.6-sol").await,
            Some(1_050_000)
        );
        assert_eq!(provider.context_window("unknown").await, None);
        assert_eq!(provider.context_window("gpt-5.6-mini").await, None);
        assert!(!provider.supports_images());
    }
}
