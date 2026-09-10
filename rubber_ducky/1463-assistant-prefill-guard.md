# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1463-assistant-prefill" -->

<!-- event id="request" artifact path="1463-assistant-prefill-guard/artifacts/round-1-review-request.diff" sha256="2338310a46babfa3efa9cea570f064e42cced14836f7d61cc058d0e9b0e2ec3e" -->
## Review Request — Round 1
**Task:** 1463 — Reject malformed tool-use responses before assistant-prefill history
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Add one provider-neutral response-shape validator enforcing ToolUse implies at least one structured ToolCall. OpenRouter invokes it in streaming and non-streaming parsers; the agent loop invokes it again before telemetry/history as defense in depth. Malformed pseudo-tool content is cleared, usage/cost is preserved, and the attempt terminates as a non-retryable typed provider error without issuing another request.

### Relevant Code / Diff
diff --git i/src/agent.rs w/src/agent.rs
index 928c76a..a727235 100644
--- i/src/agent.rs
+++ w/src/agent.rs
@@ -1071,6 +1071,10 @@ async fn run_inner(
                 tokio::time::sleep(delay).await;
             }
         };
+        // Defense in depth for every provider adapter: never let a malformed
+        // ToolUse-without-ToolCall response enter history and create an
+        // assistant-prefill request on the next loop iteration (#1463).
+        resp = crate::providers::validate_response_shape(resp);
         // Providers report reasoning tokens inconsistently, but every adapter
         // crosses this shared stream boundary. Count exact UTF-8 bytes here so
         // the metric is provider-neutral and never guessed from token ratios.
@@ -1108,11 +1112,15 @@ async fn run_inner(
             );
         }
 
-        // Assistant turn appended BEFORE tool results (Anthropic API requirement)
-        messages.push(Message {
-            role: Role::Assistant,
-            content: resp.content.clone(),
-        });
+        // Assistant turn appended BEFORE tool results (Anthropic API
+        // requirement). Shape validation clears malformed pseudo-tool content;
+        // do not persist an empty assistant husk for terminal provider errors.
+        if !resp.content.is_empty() {
+            messages.push(Message {
+                role: Role::Assistant,
+                content: resp.content.clone(),
+            });
+        }
 
         match resp.stop_reason {
             StopReason::MaxTokens
@@ -3301,6 +3309,56 @@ mod tests {
         Session::new(dir.to_path_buf(), Arc::new(Config::default()))
     }
 
+    #[tokio::test]
+    async fn malformed_tool_use_never_continues_with_assistant_prefill() {
+        let malformed_usage = Usage {
+            input: 200,
+            output: 30,
+            cost: Cost {
+                total_usd: 0.0123,
+                ..Cost::default()
+            },
+            ..Usage::default()
+        };
+        let provider = MockProvider::new(vec![
+            LlmResponse {
+                content: vec![ContentBlock::Text(
+                    "call\n<invoke name=\"read_file\"></invoke>".into(),
+                )],
+                stop_reason: StopReason::ToolUse,
+                error_message: None,
+                context_overflow: false,
+                retryable: false,
+                usage: malformed_usage,
+            },
+            end_turn_resp_with_text("must not be requested"),
+        ]);
+        let dir = tempfile::tempdir().unwrap();
+
+        let result = run(
+            &provider,
+            shared(session_in(dir.path())),
+            vec![Message::user("inspect the file")],
+            &AgentConfig::default(),
+        )
+        .await;
+
+        assert_eq!(result.stop_reason, StopReason::Error);
+        assert_eq!(
+            result.error_message.as_deref(),
+            Some("provider declared tool use but returned no structured tool call")
+        );
+        assert_eq!(result.messages.len(), 1);
+        assert_eq!(result.messages[0].role, Role::User);
+        assert_eq!(result.usage.input, 200);
+        assert!((result.usage.cost.total_usd - 0.0123).abs() < f64::EPSILON);
+        assert_eq!(
+            provider.responses.lock().unwrap().len(),
+            1,
+            "the malformed assistant turn must not trigger another provider call"
+        );
+    }
+
     fn bounded_session_in(dir: &std::path::Path) -> Session {
         let mut cfg = Config::default();
         cfg.tool_output.directory = Some(dir.join("tool-output").to_string_lossy().to_string());
diff --git i/src/providers/mod.rs w/src/providers/mod.rs
index 6ccadae..8811b28 100644
--- i/src/providers/mod.rs
+++ w/src/providers/mod.rs
@@ -256,6 +256,31 @@ impl LlmResponse {
     }
 }
 
+pub const MISSING_STRUCTURED_TOOL_CALL: &str =
+    "provider declared tool use but returned no structured tool call";
+
+/// Enforce the provider-neutral tool-use response invariant.
+///
+/// A `ToolUse` stop without a `ToolCall` cannot be dispatched or paired with a
+/// user-role `ToolResult`. Continuing would leave assistant text at the end of
+/// history, which Anthropic-compatible routes interpret as unsupported
+/// assistant prefill. Preserve accounting from the malformed attempt, but
+/// clear its pseudo-tool content and fail before it enters durable history.
+pub fn validate_response_shape(mut response: LlmResponse) -> LlmResponse {
+    let has_tool_call = response
+        .content
+        .iter()
+        .any(|block| matches!(block, ContentBlock::ToolCall { .. }));
+    if response.stop_reason == StopReason::ToolUse && !has_tool_call {
+        response.content.clear();
+        response.stop_reason = StopReason::Error;
+        response.error_message = Some(MISSING_STRUCTURED_TOOL_CALL.to_string());
+        response.context_overflow = false;
+        response.retryable = false;
+    }
+    response
+}
+
 /// Resolved provider deadlines, in the form the adapters need (vikunja #1107).
 ///
 /// Lives in the provider layer and is shared by all three adapters for the same
diff --git i/src/providers/openrouter.rs w/src/providers/openrouter.rs
index 85b2427..99a67a8 100644
--- i/src/providers/openrouter.rs
+++ w/src/providers/openrouter.rs
@@ -345,14 +345,14 @@ impl StreamState {
         }
         let stop_reason = map_finish_reason(self.finish_reason.as_deref());
         let usage = parse_usage(&self.usage);
-        LlmResponse {
+        super::validate_response_shape(LlmResponse {
             retryable: false,
             content,
             stop_reason,
             error_message: None,
             context_overflow: false,
             usage,
-        }
+        })
     }
 }
 
@@ -531,14 +531,14 @@ pub(crate) fn parse_response(body: &Value) -> LlmResponse {
             .to_string()
     });
 
-    LlmResponse {
+    super::validate_response_shape(LlmResponse {
         retryable: false,
         content,
         stop_reason,
         error_message,
         context_overflow: false,
         usage,
-    }
+    })
 }
 
 pub(crate) fn map_finish_reason(reason: Option<&str>) -> StopReason {
@@ -860,6 +860,36 @@ mod tests {
         }
     }
 
+    #[test]
+    fn parse_tool_finish_without_structured_call_is_error() {
+        let body = json!({
+            "choices": [{
+                "message": {
+                    "role": "assistant",
+                    "content": "call\n<invoke name=\"read_file\"></invoke>"
+                },
+                "finish_reason": "tool_calls"
+            }],
+            "usage": {
+                "prompt_tokens": 200,
+                "completion_tokens": 30,
+                "cost": 0.0123
+            }
+        });
+
+        let response = parse_response(&body);
+
+        assert_eq!(response.stop_reason, StopReason::Error);
+        assert_eq!(
+            response.error_message.as_deref(),
+            Some("provider declared tool use but returned no structured tool call")
+        );
+        assert!(response.content.is_empty());
+        assert!(!response.retryable);
+        assert_eq!(response.usage.input, 200);
+        assert!((response.usage.cost.total_usd - 0.0123).abs() < f64::EPSILON);
+    }
+
     #[test]
     fn parse_max_tokens_response() {
         let body = json!({
@@ -1118,6 +1148,34 @@ mod tests {
         assert!((resp.usage.cost.total_usd - 0.00042).abs() < f64::EPSILON);
     }
 
+    #[test]
+    fn stream_tool_finish_without_structured_call_is_error() {
+        let mut state = StreamState::default();
+        state.on_chunk(&json!({
+            "choices": [{
+                "delta": {"content": "call\n<invoke name=\"read_file\"></invoke>"},
+                "finish_reason": "tool_calls"
+            }],
+            "usage": {
+                "prompt_tokens": 200,
+                "completion_tokens": 30,
+                "cost": 0.0123
+            }
+        }));
+
+        let response = state.finish();
+
+        assert_eq!(response.stop_reason, StopReason::Error);
+        assert_eq!(
+            response.error_message.as_deref(),
+            Some("provider declared tool use but returned no structured tool call")
+        );
+        assert!(response.content.is_empty());
+        assert!(!response.retryable);
+        assert_eq!(response.usage.input, 200);
+        assert!((response.usage.cost.total_usd - 0.0123).abs() < f64::EPSILON);
+    }
+
     #[test]
     fn stream_empty_content_delta_not_emitted() {
         let mut state = StreamState::default();

### Known Concerns
1. Text deltas may already have streamed to a frontend before final response-shape validation clears durable content.
2. The malformed response is deliberately terminal rather than automatically retried because current retry accounting does not preserve failed-attempt usage/cost.
3. Skipping all empty assistant responses before history append slightly broadens behavior beyond this exact malformed shape.

### Specific Questions for Reviewer
1. Is the neutral invariant enforced at the correct boundaries without violating ADR-001?
2. Does clearing malformed content while preserving accounting produce honest terminal history?
3. Is terminal non-retryable handling preferable until retry accounting and stream rollback are designed?
4. Are streaming, non-streaming, and agent defense-in-depth tests sufficient?

<!-- event id="review-1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. The agent-loop history guard broadens behavior beyond the malformed shape: `if !resp.content.is_empty()` skips appending ANY empty assistant response, including legitimate empty end-turn responses from providers unrelated to this bug. This silently changes history semantics for all adapters and stop reasons. Gate the skip on the specific terminal error path (e.g., stop_reason == Error with MISSING_STRUCTURED_TOOL_CALL, or a dedicated flag set by validate_response_shape) so unrelated empty responses keep their current behavior, or add explicit tests demonstrating that skipping all empty assistant turns is safe for every stop reason.
B2. Double invocation is inconsistent and lossy: OpenRouter's parse_response already sets error_message for some paths, but validate_response_shape unconditionally overwrites error_message and clears content when the invariant trips, potentially discarding a more specific provider error message that arrived alongside finish_reason=tool_calls. Also, only OpenRouter invokes the validator in-adapter while other adapters rely solely on the agent-loop call — but agent-loop validation runs AFTER retry/delay handling and BEFORE telemetry, meaning the reasoning-byte telemetry (counted on the shared stream boundary just below) is computed on already-cleared content for OpenRouter but on raw content for other adapters' streaming paths. Make the enforcement boundary uniform: either validate in every adapter or only in the loop, and preserve any pre-existing error_message (e.g., only overwrite when None).

### Non-Blocking Suggestions
S1. Streaming UX gap acknowledged in known_concerns is real: text deltas for the pseudo-tool content may already be visible to the frontend while durable history shows nothing. Consider emitting an explicit stream-level error/retraction event when validate_response_shape trips in StreamState::finish, so frontends can mark the streamed text as invalid rather than leaving a silent mismatch between displayed and persisted state.
S2. Terminal non-retryable handling is a reasonable interim stance given retry accounting limitations, but record it as an explicit follow-up (issue reference) and consider including the discarded raw text (truncated) in the typed error or a debug log so operators can diagnose which provider/model emitted pseudo-tool syntax.
S3. Add a test asserting that a well-formed ToolUse response with a structured ToolCall passes validate_response_shape unchanged (content, usage, stop_reason preserved), and a test for the mixed case (ToolCall plus stray pseudo-tool text) to document that mixed content is accepted, not cleared.

**Verdict:** REVISE

<!-- event id="rebuttal-1" artifact path="1463-assistant-prefill-guard/artifacts/round-1-rebuttal.diff" sha256="70fe458938b455e4fe94f98a70565624c84ee03ed6414fc06442a4f049ff7754" -->
## Rebuttal — Round 1
**Position:** AGREE

### Responses to Blocking Concerns
Re B1: ACCEPT — Valid. The agent now records the malformed ToolUse predicate before validation and skips history append only for that exact response. An explicit regression proves unrelated empty EndTurn responses retain the prior empty-assistant history behavior.
Re B2: ACCEPT — Valid. Adapter-level validation was removed; the sole enforcement boundary is the provider-neutral agent loop before telemetry and history. validate_response_shape now preserves any existing provider error via get_or_insert_with. OpenRouter streaming/non-streaming tests explicitly pass parsed responses through the neutral validator.

### Revised Code / Diff
diff --git i/src/agent.rs w/src/agent.rs
index 928c76a..98e5f42 100644
--- i/src/agent.rs
+++ w/src/agent.rs
@@ -1071,6 +1071,11 @@ async fn run_inner(
                 tokio::time::sleep(delay).await;
             }
         };
+        // One provider-neutral enforcement boundary: never let a malformed
+        // ToolUse-without-ToolCall response enter history and create an
+        // assistant-prefill request on the next loop iteration (#1463).
+        let malformed_tool_use = crate::providers::tool_use_missing_structured_call(&resp);
+        resp = crate::providers::validate_response_shape(resp);
         // Providers report reasoning tokens inconsistently, but every adapter
         // crosses this shared stream boundary. Count exact UTF-8 bytes here so
         // the metric is provider-neutral and never guessed from token ratios.
@@ -1108,11 +1113,16 @@ async fn run_inner(
             );
         }
 
-        // Assistant turn appended BEFORE tool results (Anthropic API requirement)
-        messages.push(Message {
-            role: Role::Assistant,
-            content: resp.content.clone(),
-        });
+        // Assistant turn appended BEFORE tool results (Anthropic API
+        // requirement). Only the malformed response identified above is
+        // excluded; preserve existing history semantics for unrelated empty
+        // assistant responses.
+        if !malformed_tool_use {
+            messages.push(Message {
+                role: Role::Assistant,
+                content: resp.content.clone(),
+            });
+        }
 
         match resp.stop_reason {
             StopReason::MaxTokens
@@ -3301,6 +3311,82 @@ mod tests {
         Session::new(dir.to_path_buf(), Arc::new(Config::default()))
     }
 
+    #[tokio::test]
+    async fn malformed_tool_use_never_continues_with_assistant_prefill() {
+        let malformed_usage = Usage {
+            input: 200,
+            output: 30,
+            cost: Cost {
+                total_usd: 0.0123,
+                ..Cost::default()
+            },
+            ..Usage::default()
+        };
+        let provider = MockProvider::new(vec![
+            LlmResponse {
+                content: vec![ContentBlock::Text(
+                    "call\n<invoke name=\"read_file\"></invoke>".into(),
+                )],
+                stop_reason: StopReason::ToolUse,
+                error_message: None,
+                context_overflow: false,
+                retryable: false,
+                usage: malformed_usage,
+            },
+            end_turn_resp_with_text("must not be requested"),
+        ]);
+        let dir = tempfile::tempdir().unwrap();
+
+        let result = run(
+            &provider,
+            shared(session_in(dir.path())),
+            vec![Message::user("inspect the file")],
+            &AgentConfig::default(),
+        )
+        .await;
+
+        assert_eq!(result.stop_reason, StopReason::Error);
+        assert_eq!(
+            result.error_message.as_deref(),
+            Some("provider declared tool use but returned no structured tool call")
+        );
+        assert_eq!(result.messages.len(), 1);
+        assert_eq!(result.messages[0].role, Role::User);
+        assert_eq!(result.usage.input, 200);
+        assert!((result.usage.cost.total_usd - 0.0123).abs() < f64::EPSILON);
+        assert_eq!(
+            provider.responses.lock().unwrap().len(),
+            1,
+            "the malformed assistant turn must not trigger another provider call"
+        );
+    }
+
+    #[tokio::test]
+    async fn unrelated_empty_end_turn_still_appends_assistant_history() {
+        let provider = MockProvider::new(vec![LlmResponse {
+            content: vec![],
+            stop_reason: StopReason::EndTurn,
+            error_message: None,
+            context_overflow: false,
+            retryable: false,
+            usage: mock_usage(100, 0),
+        }]);
+        let dir = tempfile::tempdir().unwrap();
+
+        let result = run(
+            &provider,
+            shared(session_in(dir.path())),
+            vec![Message::user("answer")],
+            &AgentConfig::default(),
+        )
+        .await;
+
+        assert_eq!(result.stop_reason, StopReason::EndTurn);
+        assert_eq!(result.messages.len(), 2);
+        assert_eq!(result.messages[1].role, Role::Assistant);
+        assert!(result.messages[1].content.is_empty());
+    }
+
     fn bounded_session_in(dir: &std::path::Path) -> Session {
         let mut cfg = Config::default();
         cfg.tool_output.directory = Some(dir.join("tool-output").to_string_lossy().to_string());
diff --git i/src/providers/mod.rs w/src/providers/mod.rs
index 6ccadae..c6533a3 100644
--- i/src/providers/mod.rs
+++ w/src/providers/mod.rs
@@ -256,6 +256,37 @@ impl LlmResponse {
     }
 }
 
+pub const MISSING_STRUCTURED_TOOL_CALL: &str =
+    "provider declared tool use but returned no structured tool call";
+
+pub fn tool_use_missing_structured_call(response: &LlmResponse) -> bool {
+    response.stop_reason == StopReason::ToolUse
+        && !response
+            .content
+            .iter()
+            .any(|block| matches!(block, ContentBlock::ToolCall { .. }))
+}
+
+/// Enforce the provider-neutral tool-use response invariant.
+///
+/// A `ToolUse` stop without a `ToolCall` cannot be dispatched or paired with a
+/// user-role `ToolResult`. Continuing would leave assistant text at the end of
+/// history, which Anthropic-compatible routes interpret as unsupported
+/// assistant prefill. Preserve accounting from the malformed attempt, but
+/// clear its pseudo-tool content and fail before it enters durable history.
+pub fn validate_response_shape(mut response: LlmResponse) -> LlmResponse {
+    if tool_use_missing_structured_call(&response) {
+        response.content.clear();
+        response.stop_reason = StopReason::Error;
+        response
+            .error_message
+            .get_or_insert_with(|| MISSING_STRUCTURED_TOOL_CALL.to_string());
+        response.context_overflow = false;
+        response.retryable = false;
+    }
+    response
+}
+
 /// Resolved provider deadlines, in the form the adapters need (vikunja #1107).
 ///
 /// Lives in the provider layer and is shared by all three adapters for the same
@@ -544,6 +575,77 @@ pub trait LlmProvider: Send + Sync {
 mod tests {
     use super::*;
 
+    #[test]
+    fn valid_tool_use_shape_is_preserved() {
+        let response = LlmResponse {
+            content: vec![ContentBlock::ToolCall {
+                id: "call-1".into(),
+                name: "read_file".into(),
+                input: serde_json::json!({"path": "src/main.rs"}),
+            }],
+            stop_reason: StopReason::ToolUse,
+            error_message: None,
+            context_overflow: false,
+            retryable: false,
+            usage: Usage {
+                input: 12,
+                ..Usage::default()
+            },
+        };
+
+        let validated = validate_response_shape(response);
+
+        assert_eq!(validated.stop_reason, StopReason::ToolUse);
+        assert_eq!(validated.content.len(), 1);
+        assert_eq!(validated.usage.input, 12);
+        assert!(validated.error_message.is_none());
+    }
+
+    #[test]
+    fn mixed_text_and_structured_tool_use_shape_is_preserved() {
+        let response = LlmResponse {
+            content: vec![
+                ContentBlock::Text("I will inspect it.".into()),
+                ContentBlock::ToolCall {
+                    id: "call-1".into(),
+                    name: "read_file".into(),
+                    input: serde_json::json!({"path": "src/main.rs"}),
+                },
+            ],
+            stop_reason: StopReason::ToolUse,
+            error_message: None,
+            context_overflow: false,
+            retryable: false,
+            usage: Usage::default(),
+        };
+
+        let validated = validate_response_shape(response);
+
+        assert_eq!(validated.stop_reason, StopReason::ToolUse);
+        assert_eq!(validated.content.len(), 2);
+    }
+
+    #[test]
+    fn malformed_tool_use_preserves_existing_provider_error() {
+        let response = LlmResponse {
+            content: vec![ContentBlock::Text("pseudo tool syntax".into())],
+            stop_reason: StopReason::ToolUse,
+            error_message: Some("provider-specific detail".into()),
+            context_overflow: false,
+            retryable: false,
+            usage: Usage::default(),
+        };
+
+        let validated = validate_response_shape(response);
+
+        assert_eq!(validated.stop_reason, StopReason::Error);
+        assert_eq!(
+            validated.error_message.as_deref(),
+            Some("provider-specific detail")
+        );
+        assert!(validated.content.is_empty());
+    }
+
     /// vikunja #1240: a transient provider failure must be distinguishable from
     /// a fatal one *at the provider boundary*, so core can retry without ever
     /// reading provider phrasing (ADR-001). Status codes are a standard, not
diff --git i/src/providers/openrouter.rs w/src/providers/openrouter.rs
index 85b2427..770a646 100644
--- i/src/providers/openrouter.rs
+++ w/src/providers/openrouter.rs
@@ -860,6 +860,36 @@ mod tests {
         }
     }
 
+    #[test]
+    fn parse_tool_finish_without_structured_call_is_error() {
+        let body = json!({
+            "choices": [{
+                "message": {
+                    "role": "assistant",
+                    "content": "call\n<invoke name=\"read_file\"></invoke>"
+                },
+                "finish_reason": "tool_calls"
+            }],
+            "usage": {
+                "prompt_tokens": 200,
+                "completion_tokens": 30,
+                "cost": 0.0123
+            }
+        });
+
+        let response = crate::providers::validate_response_shape(parse_response(&body));
+
+        assert_eq!(response.stop_reason, StopReason::Error);
+        assert_eq!(
+            response.error_message.as_deref(),
+            Some("provider declared tool use but returned no structured tool call")
+        );
+        assert!(response.content.is_empty());
+        assert!(!response.retryable);
+        assert_eq!(response.usage.input, 200);
+        assert!((response.usage.cost.total_usd - 0.0123).abs() < f64::EPSILON);
+    }
+
     #[test]
     fn parse_max_tokens_response() {
         let body = json!({
@@ -1118,6 +1148,34 @@ mod tests {
         assert!((resp.usage.cost.total_usd - 0.00042).abs() < f64::EPSILON);
     }
 
+    #[test]
+    fn stream_tool_finish_without_structured_call_is_error() {
+        let mut state = StreamState::default();
+        state.on_chunk(&json!({
+            "choices": [{
+                "delta": {"content": "call\n<invoke name=\"read_file\"></invoke>"},
+                "finish_reason": "tool_calls"
+            }],
+            "usage": {
+                "prompt_tokens": 200,
+                "completion_tokens": 30,
+                "cost": 0.0123
+            }
+        }));
+
+        let response = crate::providers::validate_response_shape(state.finish());
+
+        assert_eq!(response.stop_reason, StopReason::Error);
+        assert_eq!(
+            response.error_message.as_deref(),
+            Some("provider declared tool use but returned no structured tool call")
+        );
+        assert!(response.content.is_empty());
+        assert!(!response.retryable);
+        assert_eq!(response.usage.input, 200);
+        assert!((response.usage.cost.total_usd - 0.0123).abs() < f64::EPSILON);
+    }
+
     #[test]
     fn stream_empty_content_delta_not_emitted() {
         let mut state = StreamState::default();

### New Points
R1. S3 was adopted with tests proving valid ToolUse and mixed Text+ToolCall content remain unchanged, including usage and stop reason.
R2. S1 remains deferred to streamed-attempt cleanup/retraction work tracked by #1419; S2 terminal handling and raw evidence are recorded in Vikunja #1463, and no pseudo-tool content is persisted or executed.
R3. Revised focused tests all pass and strict Clippy is clean.

**Requesting:** RE-REVIEW

<!-- event id="review-2" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
None.

### Responses to Prior Points
Re R1: Confirmed. The new valid_tool_use_shape_is_preserved and mixed_text_and_structured_tool_use_shape_is_preserved tests document exactly the behavior S3 requested: well-formed and mixed content pass through unchanged, including usage and stop reason. S3 is adopted.
Re R2: Acceptable. Deferring stream retraction (S1) to #1419 and recording the terminal non-retryable stance plus raw-evidence logging in #1463 satisfies the intent of both suggestions; neither blocks this change since no pseudo-tool content is persisted or executed.
Re R3: Acknowledged. Passing focused tests and clean strict Clippy are consistent with the revised diff; no further verification requested at this boundary.
B1: resolved
B2: resolved

**Verdict:** APPROVE
