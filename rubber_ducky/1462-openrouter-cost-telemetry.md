# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1462-openrouter-cost" -->

<!-- event id="request" artifact path="1462-openrouter-cost-telemetry/artifacts/round-1-review-request.diff" sha256="9ce1096ef1a1d1e917e3ca956d9e1c5edc521cd8f25aabadb32943cf6c01830e" -->
## Review Request — Round 1
**Task:** 1462 — Record OpenRouter generation cost in provider telemetry
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Parse OpenRouter’s authoritative usage.cost field into the provider-neutral Cost.total_usd for both streaming and non-streaming responses, retaining zero for absent, malformed, negative, or non-finite costs and leaving unavailable component buckets at zero.

### Relevant Code / Diff
diff --git i/src/providers/openrouter.rs w/src/providers/openrouter.rs
index 672bf52..85b2427 100644
--- i/src/providers/openrouter.rs
+++ w/src/providers/openrouter.rs
@@ -572,9 +572,17 @@ fn parse_usage(usage: &Value) -> Usage {
         thinking_bytes: 0,
         cache_read,
         cache_write,
-        // OpenRouter does not return cost in the usage object; left at zero.
-        // Fetch from /api/v1/generation?id={id} if per-run cost is needed.
-        cost: Cost::default(),
+        // OpenRouter reports the total account charge in the final usage frame
+        // for both streaming and non-streaming responses. It does not split
+        // that charge into our neutral input/output/cache buckets, so preserve
+        // the authoritative total and leave those optional components at zero.
+        cost: Cost {
+            total_usd: usage["cost"]
+                .as_f64()
+                .filter(|cost| cost.is_finite() && *cost >= 0.0)
+                .unwrap_or(0.0),
+            ..Cost::default()
+        },
     }
 }
 
@@ -900,7 +908,8 @@ mod tests {
                 "prompt_tokens": 194,
                 "completion_tokens": 50,
                 "prompt_tokens_details": {"cached_tokens": 30, "cache_write_tokens": 20},
-                "completion_tokens_details": {"reasoning_tokens": 0}
+                "completion_tokens_details": {"reasoning_tokens": 0},
+                "cost": 0.0015
             }
         });
         let resp = parse_response(&body);
@@ -912,6 +921,7 @@ mod tests {
         assert_eq!(resp.usage.reasoning_output, Some(0));
         assert_eq!(resp.usage.cache_read, 30);
         assert_eq!(resp.usage.cache_write, 20);
+        assert!((resp.usage.cost.total_usd - 0.0015).abs() < f64::EPSILON);
         assert_eq!(
             resp.usage.prompt_tokens(),
             194,
@@ -1069,6 +1079,7 @@ mod tests {
         assert_eq!(resp.usage.output, 0);
         assert_eq!(resp.usage.cache_read, 0);
         assert_eq!(resp.usage.cache_write, 0);
+        assert_eq!(resp.usage.cost.total_usd, 0.0);
     }
 
     // --- map_finish_reason ---
@@ -1098,12 +1109,13 @@ mod tests {
             .on_chunk(&json!({"choices": [{"delta": {"content": "lo"}, "finish_reason": null}]}));
         assert_eq!(e1, vec![StreamEvent::TextDelta("hel".to_string())]);
         assert_eq!(e2, vec![StreamEvent::TextDelta("lo".to_string())]);
-        state.on_chunk(&json!({"choices": [{"delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 10, "completion_tokens": 2}}));
+        state.on_chunk(&json!({"choices": [{"delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 10, "completion_tokens": 2, "cost": 0.00042}}));
         let resp = state.finish();
         assert!(matches!(&resp.content[0], ContentBlock::Text(t) if t == "hello"));
         assert_eq!(resp.stop_reason, StopReason::EndTurn);
         assert_eq!(resp.usage.input, 10);
         assert_eq!(resp.usage.output, 2);
+        assert!((resp.usage.cost.total_usd - 0.00042).abs() < f64::EPSILON);
     }
 
     #[test]

### Known Concerns
1. Only total_usd can be populated because OpenRouter does not report a neutral input/output/cache split.
2. The implementation trusts the final captured non-null usage frame, matching existing token accounting.
3. A live correctness-gated smoke measured $0.283460 after the fix versus zero before.

### Specific Questions for Reviewer
1. Is mapping usage.cost directly into total_usd correct at the provider boundary?
2. Are malformed and absent cost values handled safely?
3. Is test and live verification coverage sufficient?

<!-- event id="review-1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S1. Consider adding a unit test that exercises malformed cost inputs explicitly (e.g., a string value, a negative number, NaN/Infinity, and null) to lock in the .filter(is_finite && >= 0) guard behavior, rather than relying only on the absent-cost case asserting total_usd == 0.0.
S2. The comment removed from parse_usage mentioned that /api/v1/generation?id={id} can provide a per-run cost breakdown. Since OpenRouter does not split cost into input/output/cache buckets, consider retaining a brief pointer to that endpoint in the new comment for future work if component-level cost attribution is ever needed.

**Verdict:** APPROVE
