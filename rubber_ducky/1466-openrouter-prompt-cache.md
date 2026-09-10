# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1466-openrouter-prompt-cache" -->

<!-- event id="request" artifact path="1466-openrouter-prompt-cache/artifacts/round-1-review-request.diff" sha256="ac457ed1d8f34998617000a5726088a71916b8f4e25d87a1599c747aeb7400ae" -->
## Review Request — Round 1
**Task:** 1466 — Enable explicit OpenRouter prompt caching
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Contract: PRE-1 the existing prompt-cache flag is opt-in; PRE-2 OpenRouter model slug is anthropic/*. POST-1 enabled complete and stream requests carry exactly one ephemeral cache_control breakpoint on the latest wire message; POST-2 disabled or non-Anthropic OpenRouter requests carry none. INV-1 provider-neutral messages and defaults remain unchanged outside wire translation; INV-2 no top-level automatic cache option is sent, preserving normal OpenRouter upstream routing. Runtime construction propagates the existing flag.

### Relevant Code / Diff
diff --git i/daimonos.default.toml w/daimonos.default.toml
index 1b01717..8e94464 100644
--- i/daimonos.default.toml
+++ w/daimonos.default.toml
@@ -578,6 +578,6 @@ poll_interval_ms = 1500
 #   DAIMONOS_AGENT_BASE_URL=https://openrouter.ai/api/v1
 #   DAIMONOS_AGENT_APPROVAL_MODE=interactive    # auto | interactive | paranoid
 #   DAIMONOS_AGENT_API_KEY=sk-...
-#   DAIMONOS_AGENT_PROMPT_CACHE=off             # optional; Anthropic only, default off
+#   DAIMONOS_AGENT_PROMPT_CACHE=off             # optional; Anthropic models, default off
 #   DAIMONOS_AGENT_ALLOWED_COMMANDS=            # optional, comma-separated
 #   DAIMONOS_AGENT_DENIED_COMMANDS=exec         # optional, comma-separated
diff --git i/docs/adr/001-provider-boundary-invariant.md w/docs/adr/001-provider-boundary-invariant.md
index 3223a43..eb80d23 100644
--- i/docs/adr/001-provider-boundary-invariant.md
+++ w/docs/adr/001-provider-boundary-invariant.md
@@ -26,9 +26,9 @@ No model/provider-specific code in core, ever.
 
 This keeps provider-specific features out of core:
 
-- **Caching:** core marks *which prompt parts are stable* (provider-neutral "cacheable prefix
-  boundary"); the Anthropic provider turns that into a `cache_control` breakpoint, OpenAI-style
-  providers ignore it (they have automatic prefix caching), others no-op. Core never names
+- **Caching:** core expresses provider-neutral cache intent and stable-prefix information.
+  Anthropic and OpenRouter translate that intent into their own `cache_control` wire
+  breakpoints; native OpenAI relies on automatic prefix caching. Core never names
   `cache_control`.
 - **Usage telemetry:** neutral `Usage` uses provider-neutral field names (`cache_read`,
   `cache_write`, `input`, `output`, nested `cost` struct). Each provider maps its own response
diff --git i/docs/configuration.md w/docs/configuration.md
index c453e40..edf63bc 100644
--- i/docs/configuration.md
+++ w/docs/configuration.md
@@ -112,19 +112,24 @@ provider (pick `xhigh` unless you specifically want `max` to mean the
 maximum on a provider that exposes a distinct level). Provider defaults and
 the Anthropic adaptive thinking behavior are unchanged by this key.
 
-### Anthropic tool-prefix caching (`DAIMONOS_AGENT_PROMPT_CACHE`)
+### Provider prompt caching (`DAIMONOS_AGENT_PROMPT_CACHE`)
 
-Optional and default-off. Set `DAIMONOS_AGENT_PROMPT_CACHE=on` to place an
-ephemeral Anthropic prompt-cache breakpoint on the final tool definition,
-caching the complete stable tool-schema prefix. Other providers ignore this
-setting.
+Optional and default-off. Set `DAIMONOS_AGENT_PROMPT_CACHE=on` to enable an
+explicit ephemeral cache breakpoint:
 
-The first request pays Anthropic's cache-write premium. Repeated requests with
-the same tools then use cheaper cache reads, so this favors tool-loop turns and
-can cost more for a one-call response. A four-run Opus 4.8 Task 04 experiment
-reduced fresh input 75.3% and cost 43.1% overall; at matched three-call behavior,
-warm-cache cost fell approximately 54.7%. Keep it opt-in until the broader
-native-agent suite confirms the one-call trade-off.
+- Anthropic marks the final tool definition, caching the stable tool-schema
+  prefix.
+- OpenRouter marks the latest conversation message for `anthropic/*` models.
+  The explicit marker keeps OpenRouter's normal upstream routing; Daimonos does
+  not use the top-level cache option that restricts routing to direct
+  Anthropic. Other OpenRouter model families are left unchanged.
+- Native OpenAI ignores this setting because its prefix caching is automatic.
+
+The first request can pay a cache-write premium. Repeated requests with the
+same prefix then use cheaper cache reads, so this favors tool-loop turns and
+can cost more for a one-call response. A four-run direct-Anthropic Opus 4.8
+Task 04 experiment reduced fresh input 75.3% and cost 43.1% overall; at matched
+three-call behavior, warm-cache cost fell approximately 54.7%.
 
 ```dotenv
 DAIMONOS_AGENT_PROMPT_CACHE=on
diff --git i/src/agent_env.rs w/src/agent_env.rs
index cb6211e..1ac4e48 100644
--- i/src/agent_env.rs
+++ w/src/agent_env.rs
@@ -90,8 +90,8 @@ pub struct AgentEnv {
     pub base_url: String,
     pub approval_mode: String,
     pub api_key: String,
-    /// Explicit provider prompt caching. Optional and default-off until the
-    /// broader native-agent benchmark establishes the one-call trade-off.
+    /// Explicit prompt caching for direct Anthropic and Anthropic models through
+    /// OpenRouter. Optional and default-off because one call may pay only a write.
     pub prompt_cache: bool,
     pub allowed_commands: Vec<String>,
     pub denied_commands: Vec<String>,
diff --git i/src/agent_runtime.rs w/src/agent_runtime.rs
index 52c70b4..c51bb1c 100644
--- i/src/agent_runtime.rs
+++ w/src/agent_runtime.rs
@@ -31,7 +31,10 @@ fn try_build_provider(
                 base_url.to_string()
             },
         )
-        .map(|p| Box::new(p.with_timeouts(timeouts)) as Box<dyn providers::LlmProvider>),
+        .map(|p| {
+            Box::new(p.with_prompt_cache(prompt_cache).with_timeouts(timeouts))
+                as Box<dyn providers::LlmProvider>
+        }),
         "anthropic" => {
             let mut p = providers::anthropic::AnthropicProvider::new(api_key.to_string())
                 .with_prompt_cache(prompt_cache)
@@ -1054,6 +1057,113 @@ fn check_agent_result(result: &agent::AgentResult) -> anyhow::Result<()> {
 mod tests {
     use super::*;
 
+    async fn mock_openrouter() -> (String, tokio::sync::oneshot::Receiver<String>) {
+        use tokio::io::{AsyncReadExt, AsyncWriteExt};
+        use tokio::net::TcpListener;
+
+        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
+        let address = listener.local_addr().unwrap();
+        let (tx, rx) = tokio::sync::oneshot::channel();
+        tokio::spawn(async move {
+            let (mut socket, _) = listener.accept().await.unwrap();
+            let mut request = Vec::new();
+            let mut buffer = [0u8; 4096];
+            loop {
+                let n = socket.read(&mut buffer).await.unwrap();
+                if n == 0 {
+                    break;
+                }
+                request.extend_from_slice(&buffer[..n]);
+                let text = String::from_utf8_lossy(&request);
+                if let Some(split) = text.find("\r\n\r\n") {
+                    let length = text[..split]
+                        .lines()
+                        .find_map(|line| {
+                            line.to_ascii_lowercase()
+                                .strip_prefix("content-length:")
+                                .map(str::trim)
+                                .and_then(|value| value.parse::<usize>().ok())
+                        })
+                        .unwrap_or(0);
+                    if request.len() >= split + 4 + length {
+                        break;
+                    }
+                }
+            }
+            let _ = tx.send(String::from_utf8_lossy(&request).into_owned());
+            let body = serde_json::json!({
+                "choices": [{
+                    "message": {"role": "assistant", "content": "done"},
+                    "finish_reason": "stop"
+                }],
+                "usage": {"prompt_tokens": 10, "completion_tokens": 2}
+            })
+            .to_string();
+            let response = format!(
+                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
+                body.len()
+            );
+            socket.write_all(response.as_bytes()).await.unwrap();
+        });
+        (format!("http://{address}"), rx)
+    }
+
+    #[tokio::test]
+    async fn runtime_openrouter_prompt_cache_flag_reaches_request() {
+        let (base_url, captured) = mock_openrouter().await;
+        let provider = try_build_provider(
+            "openrouter",
+            "key",
+            &base_url,
+            true,
+            providers::ProviderTimeouts::default(),
+        )
+        .unwrap();
+        let context = providers::Context {
+            messages: vec![providers::Message::user("inspect")],
+            system: Some("system".into()),
+            tools: vec![],
+            stable_prefix_len: 0,
+        };
+
+        let options = providers::CompleteOpts {
+            model: "anthropic/claude-opus-4.8".into(),
+            ..providers::CompleteOpts::default()
+        };
+        let response = provider.complete(&context, &options).await;
+
+        assert_eq!(response.stop_reason, providers::StopReason::EndTurn);
+        let request = captured.await.unwrap();
+        assert_eq!(request.matches("\"cache_control\"").count(), 1);
+    }
+
+    #[tokio::test]
+    async fn runtime_openrouter_prompt_cache_remains_opt_in() {
+        let (base_url, captured) = mock_openrouter().await;
+        let provider = try_build_provider(
+            "openrouter",
+            "key",
+            &base_url,
+            false,
+            providers::ProviderTimeouts::default(),
+        )
+        .unwrap();
+        let context = providers::Context {
+            messages: vec![providers::Message::user("inspect")],
+            system: Some("system".into()),
+            tools: vec![],
+            stable_prefix_len: 0,
+        };
+
+        let response = provider
+            .complete(&context, &providers::CompleteOpts::default())
+            .await;
+
+        assert_eq!(response.stop_reason, providers::StopReason::EndTurn);
+        let request = captured.await.unwrap();
+        assert!(!request.contains("\"cache_control\""));
+    }
+
     /// Stub provider whose `list_models` answer is scripted.
     struct CatalogStub(Option<Vec<String>>);
 
diff --git i/src/providers/openrouter.rs w/src/providers/openrouter.rs
index 770a646..6c265bd 100644
--- i/src/providers/openrouter.rs
+++ w/src/providers/openrouter.rs
@@ -10,6 +10,7 @@ pub struct OpenRouterProvider {
     api_key: String,
     base_url: String,
     client: reqwest::Client,
+    prompt_cache: bool,
     /// Bounded HTTP/SSE deadlines (#1107).
     timeouts: super::ProviderTimeouts,
 }
@@ -24,10 +25,16 @@ impl OpenRouterProvider {
             api_key,
             base_url,
             client,
+            prompt_cache: false,
             timeouts,
         })
     }
 
+    pub fn with_prompt_cache(mut self, enabled: bool) -> Self {
+        self.prompt_cache = enabled;
+        self
+    }
+
     /// Override the bounded HTTP/SSE deadlines (#1107).
     pub fn with_timeouts(mut self, timeouts: crate::providers::ProviderTimeouts) -> Self {
         self.client = timeouts.client();
@@ -74,7 +81,10 @@ impl LlmProvider for OpenRouterProvider {
     }
 
     async fn complete(&self, ctx: &Context, opts: &CompleteOpts) -> LlmResponse {
-        let messages = messages_to_wire(ctx.system.as_deref(), &ctx.messages);
+        let mut messages = messages_to_wire(ctx.system.as_deref(), &ctx.messages);
+        if self.prompt_cache && supports_explicit_prompt_cache(&opts.model) {
+            apply_prompt_cache(&mut messages);
+        }
         let tools = tools_to_wire(&ctx.tools);
 
         let mut body = json!({
@@ -147,7 +157,10 @@ impl LlmProvider for OpenRouterProvider {
         use eventsource_stream::Eventsource;
         use futures_util::StreamExt;
 
-        let messages = messages_to_wire(ctx.system.as_deref(), &ctx.messages);
+        let mut messages = messages_to_wire(ctx.system.as_deref(), &ctx.messages);
+        if self.prompt_cache && supports_explicit_prompt_cache(&opts.model) {
+            apply_prompt_cache(&mut messages);
+        }
         let tools = tools_to_wire(&ctx.tools);
 
         // No `stream_options`: OpenRouter deprecated `include_usage` (usage
@@ -480,6 +493,45 @@ pub(crate) fn messages_to_wire(system: Option<&str>, messages: &[Message]) -> Ve
     wire
 }
 
+/// Place one Anthropic-compatible ephemeral cache breakpoint at the end of the
+/// OpenAI-format conversation. OpenRouter forwards this per-message/content
+/// extension without the upstream-routing restriction of top-level automatic
+/// cache control.
+fn apply_prompt_cache(messages: &mut [Value]) {
+    let Some(message) = messages.last_mut() else {
+        return;
+    };
+    let marker = json!({"type": "ephemeral"});
+
+    if message["role"] == "tool" || message["content"].is_null() {
+        message["cache_control"] = marker;
+        return;
+    }
+
+    match message.get_mut("content") {
+        Some(Value::String(text)) => {
+            let text = std::mem::take(text);
+            message["content"] = json!([{
+                "type": "text",
+                "text": text,
+                "cache_control": marker,
+            }]);
+        }
+        Some(Value::Array(blocks)) => {
+            if let Some(block) = blocks.last_mut() {
+                block["cache_control"] = marker;
+            } else {
+                message["cache_control"] = marker;
+            }
+        }
+        _ => message["cache_control"] = marker,
+    }
+}
+
+fn supports_explicit_prompt_cache(model: &str) -> bool {
+    model.starts_with("anthropic/")
+}
+
 /// Serialize tool schemas to the OpenAI `tools` array format.
 pub(crate) fn tools_to_wire(tools: &[ToolSchema]) -> Vec<Value> {
     tools
@@ -622,6 +674,183 @@ mod tests {
     use super::*;
     use serde_json::json;
 
+    async fn mock_server(
+        content_type: &str,
+        response_body: String,
+    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
+        use tokio::io::{AsyncReadExt, AsyncWriteExt};
+        use tokio::net::TcpListener;
+
+        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
+        let address = listener.local_addr().unwrap();
+        let (tx, rx) = tokio::sync::oneshot::channel();
+        let content_type = content_type.to_string();
+        tokio::spawn(async move {
+            let (mut socket, _) = listener.accept().await.unwrap();
+            let mut request = Vec::new();
+            let mut buffer = [0u8; 4096];
+            loop {
+                let n = socket.read(&mut buffer).await.unwrap();
+                if n == 0 {
+                    break;
+                }
+                request.extend_from_slice(&buffer[..n]);
+                let text = String::from_utf8_lossy(&request);
+                if let Some(split) = text.find("\r\n\r\n") {
+                    let length = text[..split]
+                        .lines()
+                        .find_map(|line| {
+                            line.to_ascii_lowercase()
+                                .strip_prefix("content-length:")
+                                .map(str::trim)
+                                .and_then(|value| value.parse::<usize>().ok())
+                        })
+                        .unwrap_or(0);
+                    if request.len() >= split + 4 + length {
+                        break;
+                    }
+                }
+            }
+            let _ = tx.send(String::from_utf8_lossy(&request).into_owned());
+            let response = format!(
+                "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
+                response_body.len()
+            );
+            socket.write_all(response.as_bytes()).await.unwrap();
+        });
+        (format!("http://{address}"), rx)
+    }
+
+    #[tokio::test]
+    async fn stream_prompt_cache_marks_latest_tool_result() {
+        let terminal = json!({
+            "choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}],
+            "usage": {"prompt_tokens": 10, "completion_tokens": 2}
+        });
+        let sse = format!("data: {terminal}\n\ndata: [DONE]\n\n");
+        let (base_url, captured) = mock_server("text/event-stream", sse).await;
+        let provider = OpenRouterProvider::new("key".into(), base_url)
+            .unwrap()
+            .with_prompt_cache(true);
+        let ctx = Context {
+            messages: vec![
+                Message::user("inspect"),
+                Message {
+                    role: Role::Assistant,
+                    content: vec![ContentBlock::ToolCall {
+                        id: "call_1".into(),
+                        name: "read_file".into(),
+                        input: json!({"path": "src/main.rs"}),
+                    }],
+                },
+                Message {
+                    role: Role::User,
+                    content: vec![ContentBlock::ToolResult {
+                        tool_use_id: "call_1".into(),
+                        content: "file contents".into(),
+                        is_error: false,
+                    }],
+                },
+            ],
+            system: Some("system".into()),
+            tools: vec![],
+            stable_prefix_len: 0,
+        };
+
+        let mut events = Vec::new();
+        let options = CompleteOpts {
+            model: "anthropic/claude-opus-4.8".into(),
+            ..CompleteOpts::default()
+        };
+        let response = provider
+            .stream(&ctx, &options, &mut |event| events.push(event))
+            .await;
+
+        assert_eq!(response.stop_reason, StopReason::EndTurn);
+        let request = captured.await.unwrap();
+        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
+        let messages = body["messages"].as_array().unwrap();
+        assert_eq!(messages.last().unwrap()["role"], "tool");
+        assert_eq!(
+            messages.last().unwrap()["cache_control"],
+            json!({"type": "ephemeral"})
+        );
+        assert_eq!(request.matches("\"cache_control\"").count(), 1);
+    }
+
+    #[tokio::test]
+    async fn complete_prompt_cache_marks_latest_text_message() {
+        let response = json!({
+            "choices": [{
+                "message": {"role": "assistant", "content": "done"},
+                "finish_reason": "stop"
+            }],
+            "usage": {"prompt_tokens": 10, "completion_tokens": 2}
+        })
+        .to_string();
+        let (base_url, captured) = mock_server("application/json", response).await;
+        let provider = OpenRouterProvider::new("key".into(), base_url)
+            .unwrap()
+            .with_prompt_cache(true);
+        let ctx = Context {
+            messages: vec![Message::user("inspect")],
+            system: Some("system".into()),
+            tools: vec![],
+            stable_prefix_len: 0,
+        };
+
+        let options = CompleteOpts {
+            model: "anthropic/claude-opus-4.8".into(),
+            ..CompleteOpts::default()
+        };
+        let response = provider.complete(&ctx, &options).await;
+
+        assert_eq!(response.stop_reason, StopReason::EndTurn);
+        let request = captured.await.unwrap();
+        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
+        assert_eq!(
+            body["messages"][1]["content"][0],
+            json!({
+                "type": "text",
+                "text": "inspect",
+                "cache_control": {"type": "ephemeral"}
+            })
+        );
+        assert_eq!(request.matches("\"cache_control\"").count(), 1);
+    }
+
+    #[tokio::test]
+    async fn prompt_cache_does_not_mark_non_anthropic_openrouter_models() {
+        let response = json!({
+            "choices": [{
+                "message": {"role": "assistant", "content": "done"},
+                "finish_reason": "stop"
+            }],
+            "usage": {"prompt_tokens": 10, "completion_tokens": 2}
+        })
+        .to_string();
+        let (base_url, captured) = mock_server("application/json", response).await;
+        let provider = OpenRouterProvider::new("key".into(), base_url)
+            .unwrap()
+            .with_prompt_cache(true);
+        let ctx = Context {
+            messages: vec![Message::user("inspect")],
+            system: Some("system".into()),
+            tools: vec![],
+            stable_prefix_len: 0,
+        };
+        let options = CompleteOpts {
+            model: "openai/gpt-5".into(),
+            ..CompleteOpts::default()
+        };
+
+        let response = provider.complete(&ctx, &options).await;
+
+        assert_eq!(response.stop_reason, StopReason::EndTurn);
+        let request = captured.await.unwrap();
+        assert!(!request.contains("\"cache_control\""));
+    }
+
     /// vikunja #1418: the OpenRouter adapter's mid-stream read-error path must
     /// classify a broken transport (HTTP/2 CANCEL) retryable so the turn resumes.
     #[test]

### Known Concerns
1. A tool result uses the message-level cache_control form, matching mini-swe-agent workaround behavior.
2. The latest-message strategy mirrors mini-swe-agent default_end rather than Context.stable_prefix_len, which is currently always zero in agent turns.
3. Local all-target tests have 12 known environmental failures because npm and shellcheck are absent; 1,837 other tests passed and clippy is clean.

### Specific Questions for Reviewer
1. Does explicit latest-message marking correctly cache the full prior OpenRouter prompt prefix?
2. Should cache capability remain gated to anthropic/* model slugs?
3. Are complete, stream, runtime propagation, default-off, and unsupported-model boundaries sufficiently tested?
4. Does this preserve ADR-001 provider isolation?

<!-- event id="review-1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. Message-level `cache_control` on the tool-role message is not the documented Anthropic/OpenRouter form: OpenRouter forwards Anthropic-style cache_control on content parts (multipart content blocks), and Anthropic's API only accepts cache_control on content blocks. Placing it at the message level for `role: "tool"` (as in `apply_prompt_cache` and asserted by `stream_prompt_cache_marks_latest_tool_result`) relies on an undocumented mini-swe-agent workaround that OpenRouter may silently drop or reject upstream, meaning the most common agent-loop case (latest message is a tool result) may get no caching at all. Either convert the tool message's string content into a content-block array with a `cache_control`-bearing text block, or cite OpenRouter documentation/verified wire behavior confirming message-level cache_control on tool messages is honored, and add a wire-level assertion or integration evidence for the actually-cached case.
B2. The ADR-001 update claims providers "translate provider-neutral cache intent and stable-prefix information," but the OpenRouter implementation ignores `Context.stable_prefix_len` entirely and hardcodes a latest-message strategy inside the provider. That is a provider-local caching policy decision, not a translation of core-expressed intent, which contradicts the revised ADR text as written. Either (a) have core express the breakpoint position (e.g., derive latest-message intent in core's neutral representation) so the provider genuinely translates it, or (b) rewrite the ADR/doc wording to honestly state that OpenRouter applies a provider-chosen latest-message heuristic when the neutral cache flag is on, so the documented invariant matches the code.

### Non-Blocking Suggestions
S1. Anthropic enforces a maximum of 4 cache breakpoints and cache_control on the latest message moves every turn, invalidating no prior breakpoint but creating a new cache entry each turn (each write pays the 1.25x premium on the uncached suffix). Consider documenting this per-turn write cost in docs/configuration.md so operators understand the steady-state cost profile differs from the direct-Anthropic tool-prefix strategy.
S2. `supports_explicit_prompt_cache` gates on `anthropic/` slugs only, but OpenRouter also forwards explicit cache_control for other families (e.g., google/gemini models support implicit+explicit caching via OpenRouter). Keeping the gate is fine for now, but add a code comment noting the gate is deliberately conservative and referencing the question raised, so future widening is a one-line change with context.
S3. The raw-TCP mock server is duplicated verbatim in src/agent_runtime.rs tests and src/providers/openrouter.rs tests (~60 lines each). Extract a shared test helper (e.g., a #[cfg(test)] test_support module) to avoid divergence.
S4. The `Some(Value::Array(blocks))` empty-array fallback and the `content.is_null()` branch in apply_prompt_cache are untested; add small unit tests directly on apply_prompt_cache for those edge shapes rather than relying only on end-to-end mock-server tests.

**Verdict:** REVISE

<!-- event id="rebuttal-1" artifact path="1466-openrouter-prompt-cache/artifacts/round-1-rebuttal.diff" sha256="b406aca46c96fc1f2da6bd6f5dd0511f014cb347bd878c8f63a4fb7c9bfb0bee" -->
## Rebuttal — Round 1
**Position:** AGREE

### Responses to Blocking Concerns
Re B1: ACCEPT — Tool messages now convert their string content to the documented multipart text block and place cache_control on that content block. The streamed public-interface test asserts the exact wire form and absence of a message-level marker.
Re B2: ACCEPT — ADR-001 now states honestly that OpenRouter uses a provider-local latest-cacheable-message strategy because agent turns currently provide no message boundary; no claim remains that this uses stable_prefix_len.

### Revised Code / Diff
diff --git i/daimonos.default.toml w/daimonos.default.toml
index 1b01717..8e94464 100644
--- i/daimonos.default.toml
+++ w/daimonos.default.toml
@@ -578,6 +578,6 @@ poll_interval_ms = 1500
 #   DAIMONOS_AGENT_BASE_URL=https://openrouter.ai/api/v1
 #   DAIMONOS_AGENT_APPROVAL_MODE=interactive    # auto | interactive | paranoid
 #   DAIMONOS_AGENT_API_KEY=sk-...
-#   DAIMONOS_AGENT_PROMPT_CACHE=off             # optional; Anthropic only, default off
+#   DAIMONOS_AGENT_PROMPT_CACHE=off             # optional; Anthropic models, default off
 #   DAIMONOS_AGENT_ALLOWED_COMMANDS=            # optional, comma-separated
 #   DAIMONOS_AGENT_DENIED_COMMANDS=exec         # optional, comma-separated
diff --git i/docs/adr/001-provider-boundary-invariant.md w/docs/adr/001-provider-boundary-invariant.md
index 3223a43..2b3ba66 100644
--- i/docs/adr/001-provider-boundary-invariant.md
+++ w/docs/adr/001-provider-boundary-invariant.md
@@ -26,10 +26,11 @@ No model/provider-specific code in core, ever.
 
 This keeps provider-specific features out of core:
 
-- **Caching:** core marks *which prompt parts are stable* (provider-neutral "cacheable prefix
-  boundary"); the Anthropic provider turns that into a `cache_control` breakpoint, OpenAI-style
-  providers ignore it (they have automatic prefix caching), others no-op. Core never names
-  `cache_control`.
+- **Caching:** core expresses provider-neutral cache intent and may expose stable-prefix
+  information. Anthropic marks its stable tool/prompt boundary. OpenRouter applies a
+  provider-local latest-cacheable-message strategy because its Chat Completions request
+  shape differs and agent turns currently provide no message boundary. Native OpenAI
+  relies on automatic prefix caching. Core never names `cache_control`.
 - **Usage telemetry:** neutral `Usage` uses provider-neutral field names (`cache_read`,
   `cache_write`, `input`, `output`, nested `cost` struct). Each provider maps its own response
   fields in — no Anthropic field name (`cache_read_input_tokens`, etc.) crosses the trait
diff --git i/docs/configuration.md w/docs/configuration.md
index c453e40..00ad5ee 100644
--- i/docs/configuration.md
+++ w/docs/configuration.md
@@ -112,19 +112,30 @@ provider (pick `xhigh` unless you specifically want `max` to mean the
 maximum on a provider that exposes a distinct level). Provider defaults and
 the Anthropic adaptive thinking behavior are unchanged by this key.
 
-### Anthropic tool-prefix caching (`DAIMONOS_AGENT_PROMPT_CACHE`)
+### Provider prompt caching (`DAIMONOS_AGENT_PROMPT_CACHE`)
 
-Optional and default-off. Set `DAIMONOS_AGENT_PROMPT_CACHE=on` to place an
-ephemeral Anthropic prompt-cache breakpoint on the final tool definition,
-caching the complete stable tool-schema prefix. Other providers ignore this
-setting.
+Optional and default-off. Set `DAIMONOS_AGENT_PROMPT_CACHE=on` to enable an
+explicit ephemeral cache breakpoint:
 
-The first request pays Anthropic's cache-write premium. Repeated requests with
-the same tools then use cheaper cache reads, so this favors tool-loop turns and
-can cost more for a one-call response. A four-run Opus 4.8 Task 04 experiment
-reduced fresh input 75.3% and cost 43.1% overall; at matched three-call behavior,
-warm-cache cost fell approximately 54.7%. Keep it opt-in until the broader
-native-agent suite confirms the one-call trade-off.
+- Anthropic marks the final tool definition, caching the stable tool-schema
+  prefix.
+- OpenRouter marks the final cacheable content block in the latest conversation
+  message for `anthropic/*` models. The explicit marker keeps OpenRouter's
+  normal upstream routing; Daimonos does not use the top-level cache option that
+  restricts routing to direct Anthropic. Other OpenRouter model families are
+  left unchanged.
+- Native OpenAI ignores this setting because its prefix caching is automatic.
+
+The first request can pay a cache-write premium. Repeated requests with the
+same prefix then use cheaper cache reads, so this favors tool-loop turns and
+can cost more for a one-call response. A four-run direct-Anthropic Opus 4.8
+Task 04 experiment reduced fresh input 75.3% and cost 43.1% overall; at matched
+three-call behavior, warm-cache cost fell approximately 54.7%.
+
+OpenRouter advances its breakpoint to the latest cacheable message each turn.
+The prior prefix can be read from cache, while the newly appended suffix is
+written as the next cache entry and can incur the provider's write premium.
+This differs from direct Anthropic's fixed tool-schema breakpoint.
 
 ```dotenv
 DAIMONOS_AGENT_PROMPT_CACHE=on
diff --git i/src/agent_env.rs w/src/agent_env.rs
index cb6211e..1ac4e48 100644
--- i/src/agent_env.rs
+++ w/src/agent_env.rs
@@ -90,8 +90,8 @@ pub struct AgentEnv {
     pub base_url: String,
     pub approval_mode: String,
     pub api_key: String,
-    /// Explicit provider prompt caching. Optional and default-off until the
-    /// broader native-agent benchmark establishes the one-call trade-off.
+    /// Explicit prompt caching for direct Anthropic and Anthropic models through
+    /// OpenRouter. Optional and default-off because one call may pay only a write.
     pub prompt_cache: bool,
     pub allowed_commands: Vec<String>,
     pub denied_commands: Vec<String>,
diff --git i/src/agent_runtime.rs w/src/agent_runtime.rs
index 52c70b4..1a3fe5a 100644
--- i/src/agent_runtime.rs
+++ w/src/agent_runtime.rs
@@ -31,7 +31,10 @@ fn try_build_provider(
                 base_url.to_string()
             },
         )
-        .map(|p| Box::new(p.with_timeouts(timeouts)) as Box<dyn providers::LlmProvider>),
+        .map(|p| {
+            Box::new(p.with_prompt_cache(prompt_cache).with_timeouts(timeouts))
+                as Box<dyn providers::LlmProvider>
+        }),
         "anthropic" => {
             let mut p = providers::anthropic::AnthropicProvider::new(api_key.to_string())
                 .with_prompt_cache(prompt_cache)
@@ -1054,6 +1057,74 @@ fn check_agent_result(result: &agent::AgentResult) -> anyhow::Result<()> {
 mod tests {
     use super::*;
 
+    async fn mock_openrouter() -> (String, tokio::sync::oneshot::Receiver<String>) {
+        let body = serde_json::json!({
+            "choices": [{
+                "message": {"role": "assistant", "content": "done"},
+                "finish_reason": "stop"
+            }],
+            "usage": {"prompt_tokens": 10, "completion_tokens": 2}
+        })
+        .to_string();
+        providers::test_support::mock_http_server("200 OK", "application/json", body).await
+    }
+
+    #[tokio::test]
+    async fn runtime_openrouter_prompt_cache_flag_reaches_request() {
+        let (base_url, captured) = mock_openrouter().await;
+        let provider = try_build_provider(
+            "openrouter",
+            "key",
+            &base_url,
+            true,
+            providers::ProviderTimeouts::default(),
+        )
+        .unwrap();
+        let context = providers::Context {
+            messages: vec![providers::Message::user("inspect")],
+            system: Some("system".into()),
+            tools: vec![],
+            stable_prefix_len: 0,
+        };
+
+        let options = providers::CompleteOpts {
+            model: "anthropic/claude-opus-4.8".into(),
+            ..providers::CompleteOpts::default()
+        };
+        let response = provider.complete(&context, &options).await;
+
+        assert_eq!(response.stop_reason, providers::StopReason::EndTurn);
+        let request = captured.await.unwrap();
+        assert_eq!(request.matches("\"cache_control\"").count(), 1);
+    }
+
+    #[tokio::test]
+    async fn runtime_openrouter_prompt_cache_remains_opt_in() {
+        let (base_url, captured) = mock_openrouter().await;
+        let provider = try_build_provider(
+            "openrouter",
+            "key",
+            &base_url,
+            false,
+            providers::ProviderTimeouts::default(),
+        )
+        .unwrap();
+        let context = providers::Context {
+            messages: vec![providers::Message::user("inspect")],
+            system: Some("system".into()),
+            tools: vec![],
+            stable_prefix_len: 0,
+        };
+
+        let response = provider
+            .complete(&context, &providers::CompleteOpts::default())
+            .await;
+
+        assert_eq!(response.stop_reason, providers::StopReason::EndTurn);
+        let request = captured.await.unwrap();
+        assert!(!request.contains("\"cache_control\""));
+    }
+
     /// Stub provider whose `list_models` answer is scripted.
     struct CatalogStub(Option<Vec<String>>);
 
diff --git i/src/providers/mod.rs w/src/providers/mod.rs
index c6533a3..7eb1e67 100644
--- i/src/providers/mod.rs
+++ w/src/providers/mod.rs
@@ -571,6 +571,58 @@ pub trait LlmProvider: Send + Sync {
     }
 }
 
+#[cfg(test)]
+pub(crate) mod test_support {
+    pub async fn mock_http_server(
+        status: &str,
+        content_type: &str,
+        response_body: String,
+    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
+        use tokio::io::{AsyncReadExt, AsyncWriteExt};
+        use tokio::net::TcpListener;
+
+        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
+        let address = listener.local_addr().unwrap();
+        let (tx, rx) = tokio::sync::oneshot::channel();
+        let status = status.to_string();
+        let content_type = content_type.to_string();
+        tokio::spawn(async move {
+            let (mut socket, _) = listener.accept().await.unwrap();
+            let mut request = Vec::new();
+            let mut buffer = [0u8; 4096];
+            loop {
+                let n = socket.read(&mut buffer).await.unwrap();
+                if n == 0 {
+                    break;
+                }
+                request.extend_from_slice(&buffer[..n]);
+                let text = String::from_utf8_lossy(&request);
+                if let Some(split) = text.find("\r\n\r\n") {
+                    let length = text[..split]
+                        .lines()
+                        .find_map(|line| {
+                            line.to_ascii_lowercase()
+                                .strip_prefix("content-length:")
+                                .map(str::trim)
+                                .and_then(|value| value.parse::<usize>().ok())
+                        })
+                        .unwrap_or(0);
+                    if request.len() >= split + 4 + length {
+                        break;
+                    }
+                }
+            }
+            let _ = tx.send(String::from_utf8_lossy(&request).into_owned());
+            let response = format!(
+                "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
+                response_body.len()
+            );
+            socket.write_all(response.as_bytes()).await.unwrap();
+        });
+        (format!("http://{address}"), rx)
+    }
+}
+
 #[cfg(test)]
 mod tests {
     use super::*;
diff --git i/src/providers/openrouter.rs w/src/providers/openrouter.rs
index 770a646..b337b67 100644
--- i/src/providers/openrouter.rs
+++ w/src/providers/openrouter.rs
@@ -10,6 +10,7 @@ pub struct OpenRouterProvider {
     api_key: String,
     base_url: String,
     client: reqwest::Client,
+    prompt_cache: bool,
     /// Bounded HTTP/SSE deadlines (#1107).
     timeouts: super::ProviderTimeouts,
 }
@@ -24,10 +25,16 @@ impl OpenRouterProvider {
             api_key,
             base_url,
             client,
+            prompt_cache: false,
             timeouts,
         })
     }
 
+    pub fn with_prompt_cache(mut self, enabled: bool) -> Self {
+        self.prompt_cache = enabled;
+        self
+    }
+
     /// Override the bounded HTTP/SSE deadlines (#1107).
     pub fn with_timeouts(mut self, timeouts: crate::providers::ProviderTimeouts) -> Self {
         self.client = timeouts.client();
@@ -74,7 +81,10 @@ impl LlmProvider for OpenRouterProvider {
     }
 
     async fn complete(&self, ctx: &Context, opts: &CompleteOpts) -> LlmResponse {
-        let messages = messages_to_wire(ctx.system.as_deref(), &ctx.messages);
+        let mut messages = messages_to_wire(ctx.system.as_deref(), &ctx.messages);
+        if self.prompt_cache && supports_explicit_prompt_cache(&opts.model) {
+            apply_prompt_cache(&mut messages);
+        }
         let tools = tools_to_wire(&ctx.tools);
 
         let mut body = json!({
@@ -147,7 +157,10 @@ impl LlmProvider for OpenRouterProvider {
         use eventsource_stream::Eventsource;
         use futures_util::StreamExt;
 
-        let messages = messages_to_wire(ctx.system.as_deref(), &ctx.messages);
+        let mut messages = messages_to_wire(ctx.system.as_deref(), &ctx.messages);
+        if self.prompt_cache && supports_explicit_prompt_cache(&opts.model) {
+            apply_prompt_cache(&mut messages);
+        }
         let tools = tools_to_wire(&ctx.tools);
 
         // No `stream_options`: OpenRouter deprecated `include_usage` (usage
@@ -480,6 +493,39 @@ pub(crate) fn messages_to_wire(system: Option<&str>, messages: &[Message]) -> Ve
     wire
 }
 
+/// Place one Anthropic-compatible ephemeral cache breakpoint on the final
+/// cacheable content block. This avoids the upstream-routing restriction of
+/// OpenRouter's top-level automatic cache control.
+fn apply_prompt_cache(messages: &mut [Value]) {
+    let Some(message) = messages.last_mut() else {
+        return;
+    };
+    let marker = json!({"type": "ephemeral"});
+
+    match message.get_mut("content") {
+        Some(Value::String(text)) => {
+            let text = std::mem::take(text);
+            message["content"] = json!([{
+                "type": "text",
+                "text": text,
+                "cache_control": marker,
+            }]);
+        }
+        Some(Value::Array(blocks)) => {
+            if let Some(block) = blocks.last_mut() {
+                block["cache_control"] = marker;
+            }
+        }
+        _ => {}
+    }
+}
+
+fn supports_explicit_prompt_cache(model: &str) -> bool {
+    // Conservative initial scope: this wire extension is documented for
+    // Anthropic-compatible requests. Widen only with provider-backed tests.
+    model.starts_with("anthropic/")
+}
+
 /// Serialize tool schemas to the OpenAI `tools` array format.
 pub(crate) fn tools_to_wire(tools: &[ToolSchema]) -> Vec<Value> {
     tools
@@ -620,8 +666,159 @@ pub(crate) fn context_length_from_models(body: &Value, model: &str) -> Option<u6
 #[cfg(test)]
 mod tests {
     use super::*;
+    use crate::providers::test_support::mock_http_server;
     use serde_json::json;
 
+    #[tokio::test]
+    async fn stream_prompt_cache_marks_latest_tool_result() {
+        let terminal = json!({
+            "choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}],
+            "usage": {"prompt_tokens": 10, "completion_tokens": 2}
+        });
+        let sse = format!("data: {terminal}\n\ndata: [DONE]\n\n");
+        let (base_url, captured) = mock_http_server("200 OK", "text/event-stream", sse).await;
+        let provider = OpenRouterProvider::new("key".into(), base_url)
+            .unwrap()
+            .with_prompt_cache(true);
+        let ctx = Context {
+            messages: vec![
+                Message::user("inspect"),
+                Message {
+                    role: Role::Assistant,
+                    content: vec![ContentBlock::ToolCall {
+                        id: "call_1".into(),
+                        name: "read_file".into(),
+                        input: json!({"path": "src/main.rs"}),
+                    }],
+                },
+                Message {
+                    role: Role::User,
+                    content: vec![ContentBlock::ToolResult {
+                        tool_use_id: "call_1".into(),
+                        content: "file contents".into(),
+                        is_error: false,
+                    }],
+                },
+            ],
+            system: Some("system".into()),
+            tools: vec![],
+            stable_prefix_len: 0,
+        };
+
+        let mut events = Vec::new();
+        let options = CompleteOpts {
+            model: "anthropic/claude-opus-4.8".into(),
+            ..CompleteOpts::default()
+        };
+        let response = provider
+            .stream(&ctx, &options, &mut |event| events.push(event))
+            .await;
+
+        assert_eq!(response.stop_reason, StopReason::EndTurn);
+        let request = captured.await.unwrap();
+        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
+        let messages = body["messages"].as_array().unwrap();
+        assert_eq!(messages.last().unwrap()["role"], "tool");
+        assert_eq!(
+            messages.last().unwrap()["content"][0],
+            json!({
+                "type": "text",
+                "text": "file contents",
+                "cache_control": {"type": "ephemeral"}
+            })
+        );
+        assert!(messages.last().unwrap().get("cache_control").is_none());
+        assert_eq!(request.matches("\"cache_control\"").count(), 1);
+    }
+
+    #[tokio::test]
+    async fn complete_prompt_cache_marks_latest_text_message() {
+        let response = json!({
+            "choices": [{
+                "message": {"role": "assistant", "content": "done"},
+                "finish_reason": "stop"
+            }],
+            "usage": {"prompt_tokens": 10, "completion_tokens": 2}
+        })
+        .to_string();
+        let (base_url, captured) = mock_http_server("200 OK", "application/json", response).await;
+        let provider = OpenRouterProvider::new("key".into(), base_url)
+            .unwrap()
+            .with_prompt_cache(true);
+        let ctx = Context {
+            messages: vec![Message::user("inspect")],
+            system: Some("system".into()),
+            tools: vec![],
+            stable_prefix_len: 0,
+        };
+
+        let options = CompleteOpts {
+            model: "anthropic/claude-opus-4.8".into(),
+            ..CompleteOpts::default()
+        };
+        let response = provider.complete(&ctx, &options).await;
+
+        assert_eq!(response.stop_reason, StopReason::EndTurn);
+        let request = captured.await.unwrap();
+        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
+        assert_eq!(
+            body["messages"][1]["content"][0],
+            json!({
+                "type": "text",
+                "text": "inspect",
+                "cache_control": {"type": "ephemeral"}
+            })
+        );
+        assert_eq!(request.matches("\"cache_control\"").count(), 1);
+    }
+
+    #[tokio::test]
+    async fn prompt_cache_does_not_mark_non_anthropic_openrouter_models() {
+        let response = json!({
+            "choices": [{
+                "message": {"role": "assistant", "content": "done"},
+                "finish_reason": "stop"
+            }],
+            "usage": {"prompt_tokens": 10, "completion_tokens": 2}
+        })
+        .to_string();
+        let (base_url, captured) = mock_http_server("200 OK", "application/json", response).await;
+        let provider = OpenRouterProvider::new("key".into(), base_url)
+            .unwrap()
+            .with_prompt_cache(true);
+        let ctx = Context {
+            messages: vec![Message::user("inspect")],
+            system: Some("system".into()),
+            tools: vec![],
+            stable_prefix_len: 0,
+        };
+        let options = CompleteOpts {
+            model: "openai/gpt-5".into(),
+            ..CompleteOpts::default()
+        };
+
+        let response = provider.complete(&ctx, &options).await;
+
+        assert_eq!(response.stop_reason, StopReason::EndTurn);
+        let request = captured.await.unwrap();
+        assert!(!request.contains("\"cache_control\""));
+    }
+
+    #[test]
+    fn prompt_cache_skips_messages_without_cacheable_content() {
+        for content in [Value::Null, json!([])] {
+            let mut messages = vec![json!({
+                "role": "assistant",
+                "content": content,
+                "tool_calls": [{"id": "call_1"}]
+            })];
+
+            apply_prompt_cache(&mut messages);
+
+            assert!(!messages[0].to_string().contains("cache_control"));
+        }
+    }
+
     /// vikunja #1418: the OpenRouter adapter's mid-stream read-error path must
     /// classify a broken transport (HTTP/2 CANCEL) retryable so the turn resumes.
     #[test]

### New Points
R1. Applied every suggestion: docs explain per-turn suffix write premiums; anthropic/* gating has a conservative-scope comment; duplicated TCP mocks moved to providers::test_support; null and empty content shapes are tested and intentionally left unmarked.

**Requesting:** RE-REVIEW

<!-- event id="review-2" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
None.

### Responses to Prior Points
Re R1: Verified against the revised diff. S1: docs/configuration.md now documents the per-turn moving-breakpoint behavior, cache reads of the prior prefix, and the suffix write premium, and distinguishes it from direct Anthropic's fixed tool-schema breakpoint. S2: supports_explicit_prompt_cache carries a comment stating the anthropic/* gate is deliberately conservative and should widen only with provider-backed tests. S3: the duplicated raw-TCP mock is extracted into providers::test_support::mock_http_server and both test sites consume it. S4: prompt_cache_skips_messages_without_cacheable_content covers Value::Null and empty-array content shapes, and apply_prompt_cache now intentionally no-ops instead of emitting message-level markers. All four suggestions are satisfactorily addressed.
B1: resolved
B2: resolved

**Verdict:** APPROVE
