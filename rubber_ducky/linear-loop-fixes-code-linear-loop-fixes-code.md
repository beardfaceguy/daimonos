# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="linear-loop-fixes-code" -->

<!-- event id="request" artifact path="linear-loop-fixes-code-linear-loop-fixes-code/artifacts/round-1-review-request.diff" sha256="d1e00a29e57696d617a17417f04e217228b3795a64081c71c366befc9a344cd7" -->
## Review Request — Round 1
**Task:** linear-loop-fixes-code — Routing clarity and persistent exact-error budget
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Implement approved exact-only protection, no schema adapters or dispatch coalescing. Isolated from unrelated working tree. Warn at 3 completed batches, stop at 6 identical tool/canonical args/error outcome rounds; unrelated novelty/pruning never resets; matching success clears. Explicit local timeout excluded; opaque SDK/server errors count without being labelled deterministic. 128 active fingerprint cap stops. Preserve old novelty detector. All duplicate calls still dispatch and retain results.

### Relevant Code / Diff
diff --git i/prompts/agent_system.md w/prompts/agent_system.md
index 24ac0b7..3e2a3b3 100644
--- i/prompts/agent_system.md
+++ w/prompts/agent_system.md
@@ -2,14 +2,15 @@ You are Daimonos, an agent-optimized assistant. Use the available tools to compl
 
 ## Tool efficiency rules
 
-**ALWAYS prefer `execute_script` over sequential individual tool calls.**
-When a task requires 2 or more tool operations, write a single Starlark script
+**ALWAYS prefer `execute_script` for batching native/local tool operations.**
+Remote MCP integrations are not available through `execute_script` or `tool()`: call remote MCP tools directly, using parallel calls only when independent.
+When a task requires 2 or more native/local tool operations, write a single Starlark script
 that performs all of them and set `result`. This collapses N round-trips into 1.
 
   Good: execute_script that reads three files, greps for a pattern, and writes output
   Bad:  read_file → (wait) → read_file → (wait) → search → (wait) → write_file
 
-Use individual tools only when you need exactly one operation.
+Use individual native/local tools only when you need exactly one operation; remote MCP tools use the direct path regardless of count.
 
 Each round-trip is a full inference against growing context — minimize them.
 
diff --git i/prompts/mcp_instructions.md w/prompts/mcp_instructions.md
index 6f10199..e2c4bcb 100644
--- i/prompts/mcp_instructions.md
+++ w/prompts/mcp_instructions.md
@@ -1,5 +1,5 @@
 Use daimonos tools, not built-in equivalents.
-If your plan requires 2+ tool calls, use execute_script instead — write a Starlark script that calls the tool functions and sets `result`. This is faster and cheaper than sequential calls. Only call individual tools when you need exactly one operation.
+If your plan requires 2+ native/local tool calls, use execute_script instead — write a Starlark script that calls the tool functions and sets `result`. This is faster and cheaper than sequential calls. Only call individual native/local tools when you need exactly one operation. Remote MCP integrations are not script bindings: call remote MCP tools directly, in parallel only when independent.
 Terse output. Drop filler, articles, pleasantries, hedging. Fragments OK. Technical substance exact. Code unchanged. Pattern: [thing] [action] [reason].
 File discovery: use ls(glob="*.ext", type="f", depth=N) instead of exec find — ls auto-excludes .git/node_modules/target/__pycache__ and returns structured JSON.
 Large outputs: do the work inside execute_script and set `result` to a compact answer (matching lines, a count, a summary) — not the raw payload. Keep intermediate data in sandbox variables, out of context.
diff --git i/src/acp_cmd.rs w/src/acp_cmd.rs
index 348e37c..e107c4e 100644
--- i/src/acp_cmd.rs
+++ w/src/acp_cmd.rs
@@ -1632,6 +1632,7 @@ fn build_remote_dispatch_hook(bridge_slot: BridgeSlot) -> RemoteToolHook {
                 return Some(RemoteToolResult {
                     content: reason,
                     is_error: true,
+                    transient: false,
                 });
             }
             bridge
@@ -1640,6 +1641,7 @@ fn build_remote_dispatch_hook(bridge_slot: BridgeSlot) -> RemoteToolHook {
                 .map(|outcome| RemoteToolResult {
                     content: outcome.content,
                     is_error: outcome.is_error,
+                    transient: outcome.transient,
                 })
         })
     })
diff --git i/src/agent.rs w/src/agent.rs
index e0ec176..1bc54d2 100644
--- i/src/agent.rs
+++ w/src/agent.rs
@@ -124,6 +124,7 @@ pub type ProviderNoticeHook = Box<dyn Fn(&str) + Send + Sync>;
 pub struct RemoteToolResult {
     pub content: String,
     pub is_error: bool,
+    pub transient: bool,
 }
 
 /// Dispatches a tool the opcode facade doesn't know. Consulted only when
@@ -1413,6 +1414,7 @@ async fn run_inner(
                             crate::observability::tool_kind(&name),
                         )
                     });
+                    let mut transient = false;
                     let (mut content, is_error, mut outcome) = if name == EXECUTE_SCRIPT_TOOL {
                         // execute_script shares the live session with its
                         // Starlark sandbox thread, so the loop hands it an
@@ -1646,7 +1648,10 @@ async fn run_inner(
                                     match served {
                                         // Remote MCP: the bridge emits the
                                         // `mcp.remote_tool` span with its server alias.
-                                        Some(r) => (r.content, r.is_error, None),
+                                        Some(r) => {
+                                            transient = r.transient;
+                                            (r.content, r.is_error, None)
+                                        }
                                         None => {
                                             // Emit a standalone span only when no
                                             // `tool_span` is open for this call
@@ -1825,9 +1830,11 @@ async fn run_inner(
                     // a repeated truncation placeholder is exactly the kind of
                     // no-progress signal the detector must see (vikunja #1197).
                     if loop_detector.is_some() {
-                        round_observations.push(crate::loop_detector::CallObservation::new(
+                        let mut observation = crate::loop_detector::CallObservation::new(
                             &name, &input, is_error, &content,
-                        ));
+                        );
+                        observation.transient = transient;
+                        round_observations.push(observation);
                     }
 
                     tool_results.push(ContentBlock::ToolResult {
@@ -3951,12 +3958,14 @@ mod tests {
                         return Some(RemoteToolResult {
                             content: format!("REMOTE_LARGE_SENTINEL\n{}", "l".repeat(60_000)),
                             is_error: false,
+                            transient: false,
                         });
                     }
                     name.strip_prefix("mcp__bench__medium_")
                         .map(|index| RemoteToolResult {
                             content: format!("MEDIUM_SENTINEL_{index}\n{}", "m".repeat(30_000)),
                             is_error: false,
+                            transient: false,
                         })
                 })
             })),
@@ -4887,6 +4896,7 @@ mod tests {
                     handled.then(|| RemoteToolResult {
                         content: serde_json::json!({"pid": 4242}).to_string(),
                         is_error: false,
+                        transient: false,
                     })
                 })
             })),
@@ -5067,6 +5077,7 @@ mod tests {
                     handled.then(|| RemoteToolResult {
                         content: "remote-output\n".repeat(200),
                         is_error: false,
+                        transient: false,
                     })
                 })
             })),
@@ -5213,6 +5224,126 @@ mod tests {
         }
     }
 
+    async fn exercise_remote_failure_budget(enabled: bool, transient: bool) {
+        let dir = tempfile::tempdir().unwrap();
+        let mut cfg = Config::default();
+        cfg.loop_detector.enabled = enabled;
+        let session = shared(Session::new(dir.path().to_path_buf(), Arc::new(cfg)));
+        let mut responses = Vec::new();
+        for round in 0..8 {
+            let mut response = tool_call_resp(
+                &format!("failure-{round}-a"),
+                "mcp__mock__failure",
+                json!({"query":"same"}),
+            );
+            response.content.push(ContentBlock::ToolCall {
+                id: format!("failure-{round}-b"),
+                name: "mcp__mock__failure".into(),
+                input: json!({"query":"same"}),
+            });
+            response.content.push(ContentBlock::ToolCall {
+                id: format!("lookup-{round}"),
+                name: "mcp__mock__lookup".into(),
+                input: json!({"round":round}),
+            });
+            responses.push(response);
+        }
+        responses.push(end_turn_resp());
+        let provider = BenchmarkCaptureProvider::new(responses);
+        let contexts = provider.contexts_handle();
+        let dispatched = Arc::new(Mutex::new(Vec::new()));
+        let observed = Arc::clone(&dispatched);
+        let config = AgentConfig {
+            error_resume_budget: Some(0),
+            remote_tool_dispatch: Some(Box::new(move |name: &str, _input: &Value| {
+                let name = name.to_owned();
+                let observed = Arc::clone(&observed);
+                Box::pin(async move {
+                    observed.lock().unwrap().push(name.clone());
+                    Some(RemoteToolResult {
+                        is_error: name.ends_with("failure"),
+                        content: if name.ends_with("failure") {
+                            "opaque identical error"
+                        } else {
+                            "found"
+                        }
+                        .into(),
+                        transient,
+                    })
+                })
+            })),
+            ..AgentConfig::default()
+        };
+        let result = run(
+            &provider,
+            session,
+            vec![Message::user("test failure budget")],
+            &config,
+        )
+        .await;
+        let should_stop = enabled && !transient;
+        let rounds = if should_stop { 6 } else { 8 };
+        assert_eq!(
+            contexts.lock().unwrap().len(),
+            if should_stop { 6 } else { 9 }
+        );
+        assert_eq!(
+            dispatched.lock().unwrap().len(),
+            rounds * 3,
+            "duplicates must still dispatch separately"
+        );
+        assert_eq!(
+            result.stop_reason,
+            if should_stop {
+                StopReason::Aborted
+            } else {
+                StopReason::EndTurn
+            }
+        );
+        if should_stop {
+            let reason = result.error_message.as_deref().unwrap();
+            assert!(reason.contains("mcp__mock__failure") && reason.contains("6 rounds"));
+            assert_eq!(
+                provider.responses.lock().unwrap().len(),
+                3,
+                "no seventh generation or retry"
+            );
+        }
+        let mut calls = Vec::new();
+        let mut results = Vec::new();
+        for message in &result.messages {
+            for block in &message.content {
+                match block {
+                    ContentBlock::ToolCall { id, .. } => calls.push(id.clone()),
+                    ContentBlock::ToolResult { tool_use_id, .. } => {
+                        results.push(tool_use_id.clone())
+                    }
+                    _ => {}
+                }
+            }
+        }
+        assert_eq!(calls.len(), rounds * 3);
+        assert_eq!(
+            calls, results,
+            "every call including duplicate reads retains its result ID"
+        );
+    }
+
+    #[tokio::test]
+    async fn remote_failure_budget_stops_before_next_generation_with_paired_results() {
+        exercise_remote_failure_budget(true, false).await;
+    }
+
+    #[tokio::test]
+    async fn remote_failure_budget_disabled_preserves_dispatch() {
+        exercise_remote_failure_budget(false, false).await;
+    }
+
+    #[tokio::test]
+    async fn remote_failure_budget_excludes_explicit_transient_outcomes() {
+        exercise_remote_failure_budget(true, true).await;
+    }
+
     #[tokio::test]
     async fn remote_tool_dispatch_serves_unknown_tool() {
         let dir = tempfile::tempdir().unwrap();
@@ -5228,6 +5359,7 @@ mod tests {
                     (name == "mcp__srv__echo").then(|| RemoteToolResult {
                         content: format!("echoed:{}", input["msg"].as_str().unwrap_or("")),
                         is_error: false,
+                        transient: false,
                     })
                 })
             })),
diff --git i/src/agent_mcp.rs w/src/agent_mcp.rs
index 1a6073c..96adc38 100644
--- i/src/agent_mcp.rs
+++ w/src/agent_mcp.rs
@@ -144,6 +144,7 @@ impl AgentMcp {
                     return Some(RemoteToolResult {
                         content: reason,
                         is_error: true,
+                        transient: false,
                     });
                 }
                 bridge
@@ -152,6 +153,7 @@ impl AgentMcp {
                     .map(|outcome| RemoteToolResult {
                         content: outcome.content,
                         is_error: outcome.is_error,
+                        transient: outcome.transient,
                     })
             })
         })
diff --git i/src/loop_detector.rs w/src/loop_detector.rs
index e1cf1e9..0e5c2ed 100644
--- i/src/loop_detector.rs
+++ w/src/loop_detector.rs
@@ -34,6 +34,8 @@ pub struct CallObservation {
     pub result_fp: u64,
     /// Whether the result was an error (repeated failures are a loop signal).
     pub is_error: bool,
+    /// Explicit transport timeout/failure, not inferred from result text.
+    pub transient: bool,
 }
 
 impl CallObservation {
@@ -43,17 +45,18 @@ impl CallObservation {
             call_fp: fingerprint_call(name, input),
             result_fp: fingerprint_result(is_error, result_content),
             is_error,
+            transient: false,
         }
     }
 }
 
-/// Hash of one tool call: the name plus its serialized arguments. serde_json
-/// serialization is deterministic for a given `Value`, which is sufficient —
-/// both occurrences being compared come from the same provider parse path.
+/// Hash a call with recursively sorted object keys; preserve array order.
 pub fn fingerprint_call(name: &str, input: &Value) -> u64 {
     let mut h = DefaultHasher::new();
     name.hash(&mut h);
-    input.to_string().hash(&mut h);
+    let mut canonical = input.clone();
+    canonical.sort_all_objects();
+    canonical.to_string().hash(&mut h);
     h.finish()
 }
 
@@ -93,6 +96,9 @@ pub struct LoopDetector {
     /// seen. Incremented at most once per round so a parallel batch of
     /// identical calls counts as one observation (one detector round).
     pair_rounds: HashMap<(u64, u64, bool), u32>,
+    /// Exact error pairs survive novelty and pruning until matching success.
+    /// Timestamp/request-id changes in error content deliberately do not aggregate.
+    failure_rounds: HashMap<(u64, u64), u32>,
     /// Rounds with zero novel `(call, result)` pairs, consecutively.
     consecutive_no_novelty: u32,
     /// Call-set fingerprint of the previous round.
@@ -119,6 +125,7 @@ impl LoopDetector {
             cfg,
             steer_sections,
             pair_rounds: HashMap::new(),
+            failure_rounds: HashMap::new(),
             consecutive_no_novelty: 0,
             last_call_set: None,
             steered_call_set: None,
@@ -148,6 +155,38 @@ impl LoopDetector {
         }
         self.stats.rounds_observed += 1;
 
+        // Matching successes reset only their own operation, never other tools.
+        for o in round.iter().filter(|o| !o.is_error) {
+            self.failure_rounds
+                .retain(|(call, _), _| *call != o.call_fp);
+        }
+        let mut failures: Vec<_> = round
+            .iter()
+            .filter(|o| o.is_error && !o.transient)
+            .map(|o| (o.call_fp, o.result_fp))
+            .collect();
+        failures.sort_unstable();
+        failures.dedup();
+        let mut failure_steer = None;
+        for pair in failures {
+            if !self.failure_rounds.contains_key(&pair) && self.failure_rounds.len() >= 128 {
+                return RoundVerdict::Break("exact failure budget exhausted: 128 distinct failed calls in this turn; stopping rather than evicting protection".into());
+            }
+            let count = self.failure_rounds.entry(pair).or_insert(0);
+            *count += 1;
+            let name = &round.iter().find(|o| o.call_fp == pair.0).unwrap().name;
+            if *count >= 6 {
+                return RoundVerdict::Break(format!("exact failure budget: tool {name} returned the same error in {count} rounds. Stopping this turn; change the request or start a new turn."));
+            }
+            if *count == 3 {
+                failure_steer = Some(format!("Tool {name} returned the same error in 3 rounds. Do not repeat unchanged inputs; inspect the error or ask the user for clarification."));
+            }
+        }
+        if let Some(text) = failure_steer {
+            self.stats.steers_emitted += 1;
+            return RoundVerdict::Steer(text);
+        }
+
         // A parallel batch aggregates into ONE detector round: each distinct
         // pair increments its round-count once, however many duplicates the
         // batch carried.
@@ -300,6 +339,204 @@ mod tests {
         vec![obs("read_file", &json!({"path": "a.rs"}), false, "content")]
     }
 
+    #[test]
+    fn recovered_linear_trace_stops_before_twenty_eighth_round() {
+        // Calls/errors from recovered history; successful lookup payloads redacted.
+        let trace: Vec<Vec<Value>> =
+            serde_json::from_str(include_str!("../tests/fixtures/linear_retry_loop.json")).unwrap();
+        assert_eq!(trace.len(), 28);
+        let mut d = detector();
+        let stopped = trace.iter().position(|round| {
+            let observations: Vec<_> = round
+                .iter()
+                .map(|call| {
+                    obs(
+                        call["name"].as_str().unwrap(),
+                        &call["input"],
+                        call["is_error"].as_bool().unwrap(),
+                        call["content"].as_str().unwrap(),
+                    )
+                })
+                .collect();
+            matches!(d.observe_round(&observations), RoundVerdict::Break(_))
+        });
+        assert_eq!(stopped, Some(18), "sixth exact-repeat occurs in round 19");
+    }
+
+    #[test]
+    fn exact_failures_survive_novel_rounds_and_pruning() {
+        let mut d = detector();
+        for i in 0..6 {
+            let failure = obs(
+                "mcp__linear__list_initiatives",
+                &json!({"customView":"/initiatives"}),
+                true,
+                "unknown custom view",
+            );
+            let verdict = d.observe_round(&[
+                failure.clone(),
+                failure,
+                obs("lookup", &json!({"query":i}), false, "found"),
+            ]);
+            if i == 5 {
+                assert!(
+                    matches!(verdict, RoundVerdict::Break(ref text) if text.contains("list_initiatives") && text.contains("6"))
+                );
+            } else {
+                assert!(!matches!(verdict, RoundVerdict::Break(_)));
+            }
+            d.on_context_pruned();
+        }
+    }
+
+    #[test]
+    fn exact_failure_success_resets_only_matching_call() {
+        let mut d = detector();
+        let failure = obs("remote", &json!({"query":"a"}), true, "error");
+        for i in 0..5 {
+            d.observe_round(&[failure.clone(), obs("novel", &json!({"i":i}), false, "ok")]);
+        }
+        d.observe_round(&[obs("remote", &json!({"query":"a"}), false, "ok")]);
+        assert!(!matches!(
+            d.observe_round(&[failure]),
+            RoundVerdict::Break(_)
+        ));
+    }
+
+    #[test]
+    fn exact_failure_changed_arguments_do_not_aggregate() {
+        let mut d = detector();
+        for i in 0..20 {
+            assert_eq!(
+                d.observe_round(&[obs("remote", &json!({"query":i}), true, "error")]),
+                RoundVerdict::Proceed
+            );
+        }
+    }
+
+    #[test]
+    fn exact_failure_transient_and_polling_do_not_accumulate() {
+        let mut d = detector();
+        for i in 0..12 {
+            let mut transient = obs("remote", &json!({}), true, "timeout");
+            transient.transient = true;
+            let verdict = d.observe_round(&[
+                transient,
+                obs("poll", &json!({}), false, "pending"),
+                obs("lookup", &json!({"i":i}), false, "ok"),
+            ]);
+            assert!(!matches!(verdict, RoundVerdict::Break(_)));
+        }
+        assert!(d.failure_rounds.is_empty());
+    }
+
+    #[test]
+    fn exact_failure_interleaved_tools_and_new_turn() {
+        let mut d = detector();
+        for i in 0..10 {
+            let name = if i % 2 == 0 { "a" } else { "b" };
+            assert!(!matches!(
+                d.observe_round(&[obs(
+                    name,
+                    &json!({}),
+                    true,
+                    "timeout text from opaque server"
+                )]),
+                RoundVerdict::Break(_)
+            ));
+        }
+        assert!(matches!(
+            d.observe_round(&[obs(
+                "a",
+                &json!({}),
+                true,
+                "timeout text from opaque server"
+            )]),
+            RoundVerdict::Break(_)
+        ));
+        assert_eq!(
+            detector().observe_round(&[obs(
+                "a",
+                &json!({}),
+                true,
+                "timeout text from opaque server"
+            )]),
+            RoundVerdict::Proceed
+        );
+    }
+
+    #[test]
+    fn exact_failure_changed_result_does_not_aggregate() {
+        let mut d = detector();
+        for i in 0..20 {
+            assert!(!matches!(
+                d.observe_round(&[obs("remote", &json!({}), true, &format!("error {i}"))]),
+                RoundVerdict::Break(_)
+            ));
+        }
+    }
+
+    #[test]
+    fn exact_failure_canonical_arguments_match() {
+        let a: Value = serde_json::from_str(r#"{"a":1,"b":{"c":2,"d":3}}"#).unwrap();
+        let b: Value = serde_json::from_str(r#"{"b":{"d":3,"c":2},"a":1}"#).unwrap();
+        assert_eq!(
+            fingerprint_call("remote", &a),
+            fingerprint_call("remote", &b)
+        );
+    }
+
+    #[test]
+    fn exact_failure_mixed_batch_resets_then_counts_failure_once() {
+        // Results are aggregated, not a guaranteed completion chronology:
+        // a matching success clears prior rounds, then batch failures count once.
+        for success_first in [true, false] {
+            let mut d = detector();
+            let failure = obs("remote", &json!({}), true, "error");
+            for i in 0..5 {
+                d.observe_round(&[failure.clone(), obs("lookup", &json!({"i":i}), false, "ok")]);
+            }
+            let success = obs("remote", &json!({}), false, "ok");
+            let batch = if success_first {
+                vec![success, failure.clone()]
+            } else {
+                vec![failure.clone(), success]
+            };
+            assert!(!matches!(d.observe_round(&batch), RoundVerdict::Break(_)));
+            assert_eq!(
+                d.failure_rounds.get(&(failure.call_fp, failure.result_fp)),
+                Some(&1)
+            );
+            for i in 0..4 {
+                assert!(!matches!(
+                    d.observe_round(&[
+                        failure.clone(),
+                        obs("lookup", &json!({"i":i+10}), false, "ok")
+                    ]),
+                    RoundVerdict::Break(_)
+                ));
+            }
+            assert!(matches!(
+                d.observe_round(&[failure]),
+                RoundVerdict::Break(_)
+            ));
+        }
+    }
+
+    #[test]
+    fn exact_failure_keys_are_bounded() {
+        let mut d = detector();
+        for i in 0..128 {
+            assert!(!matches!(
+                d.observe_round(&[obs("remote", &json!({"query":i}), true, "error")]),
+                RoundVerdict::Break(_)
+            ));
+        }
+        assert!(
+            matches!(d.observe_round(&[obs("remote", &json!({"query":128}), true, "error")]), RoundVerdict::Break(ref text) if text.contains("128"))
+        );
+    }
+
     #[test]
     fn identical_call_and_result_steers_at_threshold() {
         let mut d = detector();
diff --git i/src/mcp_bridge.rs w/src/mcp_bridge.rs
index 5e594fe..8852369 100644
--- i/src/mcp_bridge.rs
+++ w/src/mcp_bridge.rs
@@ -448,6 +448,8 @@ fn metadata_is_executable(_metadata: &std::fs::Metadata) -> bool {
 pub struct RemoteToolOutcome {
     pub content: String,
     pub is_error: bool,
+    /// Only explicit local timeout is classified; opaque SDK/server errors stay unknown.
+    pub transient: bool,
 }
 
 /// Maps an exposed (namespaced) tool name to the client that serves it and the
@@ -882,6 +884,7 @@ impl McpBridge {
                     RemoteToolOutcome {
                         content: format!("remote MCP tool '{name}' failed: {e}"),
                         is_error: true,
+                        transient: false,
                     },
                     crate::observability::ToolStatus::Error,
                 ),
@@ -892,6 +895,7 @@ impl McpBridge {
                             self.call_timeout.as_secs()
                         ),
                         is_error: true,
+                        transient: true,
                     },
                     crate::observability::ToolStatus::Timeout,
                 ),
@@ -1252,7 +1256,11 @@ fn result_to_outcome(result: CallToolResult) -> RemoteToolOutcome {
     } else {
         serde_json::to_string(&result.content).unwrap_or_default()
     };
-    RemoteToolOutcome { content, is_error }
+    RemoteToolOutcome {
+        content,
+        is_error,
+        transient: false,
+    }
 }
 
 /// No-op client handler: daimonos is a pure tool consumer, so it declines
@@ -1531,6 +1539,26 @@ mod tests {
         };
         let outcome = result_to_outcome(result);
         assert!(outcome.is_error);
+        assert!(
+            !outcome.transient,
+            "opaque server errors are not transport timeouts"
+        );
+    }
+
+    #[test]
+    fn opaque_timeout_text_is_not_transient() {
+        use rust_mcp_sdk::schema::TextContent;
+        let result = CallToolResult {
+            content: vec![ContentBlock::TextContent(TextContent::new(
+                "connection timed out".into(),
+                None,
+                None,
+            ))],
+            is_error: Some(true),
+            meta: None,
+            structured_content: None,
+        };
+        assert!(!result_to_outcome(result).transient);
     }
 
     // --- build: fail-open + disabled ---
diff --git i/src/prompts.rs w/src/prompts.rs
index ffcfc86..dfa63dd 100644
--- i/src/prompts.rs
+++ w/src/prompts.rs
@@ -408,6 +408,15 @@ mod tests {
         );
     }
 
+    #[test]
+    fn batching_prompts_exclude_remote_mcp_tools() {
+        for prompt in [AGENT_SYSTEM_DEFAULT, MCP_INSTRUCTIONS_DEFAULT] {
+            assert!(prompt.contains("native/local"));
+            assert!(prompt.contains("remote MCP"));
+            assert!(prompt.contains("direct"));
+        }
+    }
+
     #[test]
     fn agent_system_skips_routine_plans_and_coordination_overhead() {
         let prompt = AGENT_SYSTEM_DEFAULT.to_lowercase();
diff --git i/src/script.rs w/src/script.rs
index fb90d9d..e66cbb7 100644
--- i/src/script.rs
+++ w/src/script.rs
@@ -696,7 +696,8 @@ fn dispatch_request(request: Request, label: &str) -> Result<Response, anyhow::E
 
 /// Dispatch any native tool by name. Opcode-backed tools take the compact op
 /// path; plugin and meta tools fall through to the same shared dispatcher used
-/// by MCP and the agent loop. Keeping `tool("…")` universal prevents the model
+/// by MCP and the agent loop (not remote MCP integrations). Keeping native
+/// `tool("…")` universal prevents the model
 /// from spending retry turns learning which names require dedicated bindings.
 fn dispatch_tool_by_name(name: &str, args: &serde_json::Value) -> Result<Response, anyhow::Error> {
     let resp = match tools::build_request(name, args) {
@@ -710,6 +711,12 @@ fn dispatch_tool_by_name(name: &str, args: &serde_json::Value) -> Result<Respons
                 let Some((content, is_error, meta)) =
                     crate::mcp::dispatch_local_tool(&mut session, name, args).await
                 else {
+                    if name.starts_with("mcp__") {
+                        return Err(anyhow::anyhow!(
+                            "remote MCP tool '{name}' is unavailable in execute_script: \
+                             tool() supports native/local tools only; call this tool directly"
+                        ));
+                    }
                     return Err(anyhow::anyhow!("unknown tool '{name}'"));
                 };
 
@@ -1665,6 +1672,20 @@ mod tests {
         );
     }
 
+    #[tokio::test]
+    async fn remote_tool_in_script_explains_direct_call_boundary() {
+        let error = execute(
+            "result = tool(\"mcp__linear__get_user\", query=\"Patrick Clawson\")",
+            test_session(),
+            Duration::from_secs(5),
+        )
+        .await
+        .expect_err("remote integrations are not script bindings");
+        assert!(error.contains("mcp__linear__get_user"));
+        assert!(error.contains("native/local"));
+        assert!(error.contains("direct"));
+    }
+
     #[tokio::test]
     async fn execute_simple_expression() {
         let session = test_session();

diff --git 1/tests/fixtures/linear_retry_loop.json 2/tests/fixtures/linear_retry_loop.json
new file mode 100644
index 0000000..3081440
--- /dev/null
+++ 2/tests/fixtures/linear_retry_loop.json
@@ -0,0 +1,996 @@
+[
+  [
+    {
+      "name": "execute_script",
+      "input": {
+        "code": "def main():\n    return {\"user\": tool(\"mcp__linear__get_user\", query=\"Patrick Clawson\"), \"team\": tool(\"mcp__linear__get_team\", query=\"Infra\"), \"initiative\": tool(\"mcp__linear__get_initiative\", query=\"Infrastructure\", includeProjects=False, includeSubInitiatives=False)}\nresult = main()",
+        "timeout": 60
+      },
+      "is_error": true,
+      "content": "eval error: Traceback (most recent call last):\n  * script:3, in <module>\n      result = main()\n  * script:2, in main\n      return {\"user\": tool(\"mcp__linear__get_user\", query=\"Patrick Clawson\"), \"team...\nerror: unknown tool 'mcp__linear__get_user'\n --> script:2:21\n  |\n2 |     return {\"user\": tool(\"mcp__linear__get_user\", query=\"Patrick Clawson\"), \"team\": tool(\"mcp__linear__get_team\", query=\"Infra\"), \"initiative\": tool(\"mcp__linear__get_initiative\", query=\"Infrastructure\", includeProjects=False, includeSubInitiatives=False)}\n  |                     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^\n  |\n"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__get_user",
+      "input": {
+        "query": "Patrick Clawson"
+      },
+      "is_error": false,
+      "content": "successful lookup (redacted)"
+    },
+    {
+      "name": "mcp__linear__get_team",
+      "input": {
+        "query": "Infra"
+      },
+      "is_error": false,
+      "content": "successful lookup (redacted)"
+    },
+    {
+      "name": "mcp__linear__get_initiative",
+      "input": {
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "query": "Infrastructure"
+      },
+      "is_error": true,
+      "content": "Error: Could not find initiative \"Infrastructure\". The name matches more than one initiative, so pass its ID"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "Infrastructure",
+        "fields": [
+          "id",
+          "name",
+          "summary",
+          "url",
+          "leadTeam"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"Infrastructure\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": " ",
+        "fields": [
+          "id",
+          "name",
+          "summary",
+          "url",
+          "leadTeam"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Input validation error: Invalid arguments for tool list_initiatives: customView: Too small: expected string to have >=1 characters"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "all",
+        "fields": [
+          "id",
+          "name",
+          "summary",
+          "url",
+          "leadTeam"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"all\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/",
+        "fields": [
+          "id",
+          "name",
+          "summary",
+          "url",
+          "leadTeam"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "Infrastructure",
+        "fields": [
+          "id",
+          "name",
+          "summary",
+          "url",
+          "leadTeam"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "7c985836-2c13-4aa7-ac9e-bc84e519ad15",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"Infrastructure\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "00000000-0000-0000-0000-000000000000",
+        "fields": [
+          "id",
+          "name",
+          "summary",
+          "url",
+          "leadTeam"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"00000000-0000-0000-0000-000000000000\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "Infrastructure",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"Infrastructure\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "Infrastructure",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"Infrastructure\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "Infrastructure",
+        "fields": [
+          "id",
+          "name"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"Infrastructure\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "Infrastructure",
+        "fields": [
+          "id",
+          "name"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"Infrastructure\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "Infrastructure",
+        "fields": [
+          "id",
+          "name"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"Infrastructure\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    },
+    {
+      "name": "mcp__linear__list_custom_views",
+      "input": {
+        "cursor": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "query": "",
+        "shared": true,
+        "team": "",
+        "type": "Initiative"
+      },
+      "is_error": false,
+      "content": "successful lookup (redacted)"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    },
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    },
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    },
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    },
+    {
+      "name": "mcp__linear__get_workspace",
+      "input": {},
+      "is_error": false,
+      "content": "successful lookup (redacted)"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    },
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    },
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    },
+    {
+      "name": "mcp__linear__get_initiative",
+      "input": {
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "query": "https://linear.app/meetalix/initiative/infrastructure"
+      },
+      "is_error": true,
+      "content": "Error: Could not find initiative \"https://linear.app/meetalix/initiative/infrastructure\""
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    },
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ]
+]

### Known Concerns
1. Scope excludes schema-fidelity tests and diagnostics; those remain separately deferred.
2. Full Rust 1905 passed, 12 failed, 10 ignored; failures confined to missing npm/shellcheck. Full pytest 335 passed,15 failed,1 skipped in same missing-tool modules. Available Rust groups 1897 passed,10 ignored,20 filtered; available pytest 333 passed,1 skipped. fmt/clippy clean in isolated worktree.
3. Argument/result variants do not aggregate. Unknown identical errors can be intentional retries; explicit stop explains new-turn option.
4. New guard returns steer before legacy observation on third-repeat round; inspect interaction. Existing detector configuration exposes enabled but new 3/6/128 limits hardcoded.

### Specific Questions for Reviewer
1. Find correctness/regression issues, particularly reset ordering, transient propagation, no-next-provider-call stop, state bounds and correlation.
2. Does recovered 28-round fixture/test substantiate expected stop at round 19?
3. Identify blockers before two scoped commits, not speculative schema fixes.

<!-- event id="linear-loop-code-review-1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. Reset ordering bug in observe_round: the success-reset pass runs before failures are counted, but both operate on the same aggregated round. The mixed-batch test asserts that a matching success in the same batch clears prior rounds and then the batch's failure counts once (resetting to 1). However, a tool that alternates success+failure in every round (e.g., a flaky endpoint where one duplicate succeeds and one fails per batch) will have its counter perpetually reset to 1 and never accumulate — but more importantly, the inverse hazard: a call that fails 5 times then succeeds-and-fails in one batch gets reset then re-counted, which is defensible; yet a call that SUCCEEDS first in wall-clock time and then fails 6 times across subsequent rounds while also emitting one success per round never stops, reintroducing the unbounded-loop class this change is meant to fix. At minimum document/justify this in code and add a test for the persistent interleaved success+failure pattern; otherwise order the passes so a failure observed in the same round as its matching success still accumulates when the pattern repeats across rounds.
B2. The 128-cap Break returns BEFORE incrementing any counts for the current round and iterates `failures` in sorted-fingerprint order, so which pairs get recorded before the cap trips is nondeterministic with respect to call order (fingerprints are hash values). The turn stops, so state loss is bounded, but the Break fires even when the round ALSO contains a pair that is one increment away from the 6-round stop — masking the more actionable 'same error 6 rounds' message with the generic cap message. Also, `failure_rounds` has no reset at turn boundaries shown in this diff: confirm the detector is per-turn (constructed fresh each turn) — the `exact_failure_interleaved_tools_and_new_turn` test constructs a new detector manually, which proves nothing about the production loop. Please point to or add the production-path reset/scoping evidence (e.g., an agent.rs test asserting the budget does not leak across turns in run()).
B3. fingerprint_call now canonicalizes via sort_all_objects, but the legacy pair_rounds/novelty path uses the same fingerprint_call — changing fingerprints for the existing detector is a behavioral change to the legacy detector that the task scoped as 'preserve old novelty detector'. Key-order-differing calls that previously counted as distinct now aggregate in the legacy steer/break logic too. This may be desirable, but it widens scope beyond 'exact-only protection, no coalescing'; either split canonicalization to the failure path only, or explicitly document and test the legacy-detector behavior change (e.g., a test showing legacy steer thresholds with reordered keys) and note it in the commit message.
B4. The new failure-budget guard early-returns Steer/Break BEFORE the legacy observation block runs (self.stats and pair_rounds/no-novelty tracking are skipped for that round). On the third-repeat round the Steer return means the legacy detector never observes that round at all: its pair_rounds counters and consecutive_no_novelty are silently frozen, and steered_call_set is not updated, so legacy escalation logic may later fire late or double-steer the same pattern. The request flags this interaction ('new guard returns steer before legacy observation; inspect interaction') but no test covers the combined sequence where the same pair would trigger both detectors. Add a test for a pattern that crosses both the failure-steer (count==3) and the legacy identical-pair steer threshold, asserting no duplicate/contradictory steers and that a legacy Break still eventually fires if the failure budget doesn't.
B5. transient propagation is incomplete for execute_script-mediated failures: observation.transient is set only from the remote dispatch result path; the EXECUTE_SCRIPT branch and local dispatch always leave transient=false. That is fine for the stated scope (only explicit remote timeout is transient), but the agent.rs wiring sets `let mut transient = false` once per call and only mutates it in the Some(r) remote arm — verify the denied-permission early-return arm in build_remote_dispatch_hook/agent_mcp (transient: false, is_error: true) is intended to COUNT toward the budget: a model repeatedly calling a permission-denied tool with identical args will now hard-stop the turn at 6 rounds with a message suggesting 'change the request', which may be correct but differs from prior behavior and is untested. Add a test or an explicit comment choosing this semantics.
B6. Fixture/stop-round substantiation is not airtight for the 'expected stop at round 19' question. The fixture asserts Break at index 18 (round 19), but the counted pair is the '/initiatives' + fields [id,name,url] variant, whose identical repeats occur at trace indices 13,14,15,16,17,18(+dup),19... — index 18's round contains two duplicates which the dedup collapses to one increment, so the 6th increment lands at index 18 only if the index-13 occurrence (which coexists with a successful list_custom_views in the SAME round at index 13) is counted and if the earlier 'Infrastructure' customView variants are correctly excluded as different fingerprints. The in-round success at index 13 is list_custom_views (different call_fp) so it must NOT reset — the test passes only if retain() matches on call_fp as written. This is correct per the code, but the fixture test asserts only the stop index, not WHICH pair tripped or the counter trajectory; a regression that shifts counting by one round for a different reason could still land on 18 or silently move to 19 without diagnosis. Strengthen the fixture test to assert the Break message names mcp__linear__list_initiatives and '6 rounds', mirroring the agent-level assertion.

### Non-Blocking Suggestions
S1. The hardcoded 3/6/128 limits sit next to a config-exposed `loop_detector.enabled`; consider at least named constants (WARN_ROUNDS, STOP_ROUNDS, MAX_TRACKED_FAILURES) with doc comments so the future config plumbing is a one-line change, and so the Break/Steer messages can interpolate the same constants instead of literal '6 rounds'/'128'.
S2. The 128-cap Break message 'stopping rather than evicting protection' will be user-visible; consider including the most-repeated failing tool name to make the stop actionable, since 128 distinct failures usually still cluster on one tool.
S3. sort_all_objects clones the whole input Value per fingerprint; for large execute_script code payloads this is a per-call allocation on the hot path. Hashing via a recursive visitor over &Value (sorting key references only) avoids the clone. Non-blocking given typical input sizes.
S4. In exercise_remote_failure_budget, the dispatched-count assertion `rounds * 3` and context-count `6 vs 9` encode the stop-before-next-generation property implicitly; a brief comment mapping rounds→contexts (initial + per-round regenerations) would help future readers verify the no-seventh-generation claim without re-deriving it.
S5. prompts test `batching_prompts_exclude_remote_mcp_tools` checks substrings only; consider asserting the negative too (e.g., the old unconditional 'ALWAYS prefer execute_script over sequential individual tool calls' phrasing is gone) so a partial revert fails the test.

**Verdict:** REVISE

<!-- event id="linear-loop-code-rebut-1" artifact path="linear-loop-fixes-code-linear-loop-fixes-code/artifacts/round-1-rebuttal.diff" sha256="62fba3a03d5dbe0c879911968275cff0640d5f08d0a3f76e83c2ee961a70445c" -->
## Rebuttal — Round 1
**Position:** AGREE

### Responses to Blocking Concerns
Re B1: ACCEPT — Document matching success as progress including mixed batches; persistent mixed-pattern test added. Legacy guard remains independent.
Re B2: ACCEPT — Existing counters precede new keys; sixth-repeat beats cap, tested. Production AgentSession two-turn test confirms fresh detector/no auto-resume.
Re B3: ACCEPT — Explicitly include canonicalization in legacy semantics; reordered-key nonerror legacy steer test added. Commit will document it.
Re B4: ACCEPT — Legacy observation always runs on warning rounds. One steer with legacy Break priority. Shared steer state updated. Test verifies pair count 3 and exactly one steer.
Re B5: ACCEPT — Permission denials intentionally nontransient. Production denial-stop/reset test added. Script/native opaque errors not inferred transient.
Re B6: ACCEPT — Recovered fixture asserts tool name and six-round reason as well as round 19.

### Revised Code / Diff
diff --git i/prompts/agent_system.md w/prompts/agent_system.md
index 24ac0b7..3e2a3b3 100644
--- i/prompts/agent_system.md
+++ w/prompts/agent_system.md
@@ -2,14 +2,15 @@ You are Daimonos, an agent-optimized assistant. Use the available tools to compl
 
 ## Tool efficiency rules
 
-**ALWAYS prefer `execute_script` over sequential individual tool calls.**
-When a task requires 2 or more tool operations, write a single Starlark script
+**ALWAYS prefer `execute_script` for batching native/local tool operations.**
+Remote MCP integrations are not available through `execute_script` or `tool()`: call remote MCP tools directly, using parallel calls only when independent.
+When a task requires 2 or more native/local tool operations, write a single Starlark script
 that performs all of them and set `result`. This collapses N round-trips into 1.
 
   Good: execute_script that reads three files, greps for a pattern, and writes output
   Bad:  read_file → (wait) → read_file → (wait) → search → (wait) → write_file
 
-Use individual tools only when you need exactly one operation.
+Use individual native/local tools only when you need exactly one operation; remote MCP tools use the direct path regardless of count.
 
 Each round-trip is a full inference against growing context — minimize them.
 
diff --git i/prompts/mcp_instructions.md w/prompts/mcp_instructions.md
index 6f10199..e2c4bcb 100644
--- i/prompts/mcp_instructions.md
+++ w/prompts/mcp_instructions.md
@@ -1,5 +1,5 @@
 Use daimonos tools, not built-in equivalents.
-If your plan requires 2+ tool calls, use execute_script instead — write a Starlark script that calls the tool functions and sets `result`. This is faster and cheaper than sequential calls. Only call individual tools when you need exactly one operation.
+If your plan requires 2+ native/local tool calls, use execute_script instead — write a Starlark script that calls the tool functions and sets `result`. This is faster and cheaper than sequential calls. Only call individual native/local tools when you need exactly one operation. Remote MCP integrations are not script bindings: call remote MCP tools directly, in parallel only when independent.
 Terse output. Drop filler, articles, pleasantries, hedging. Fragments OK. Technical substance exact. Code unchanged. Pattern: [thing] [action] [reason].
 File discovery: use ls(glob="*.ext", type="f", depth=N) instead of exec find — ls auto-excludes .git/node_modules/target/__pycache__ and returns structured JSON.
 Large outputs: do the work inside execute_script and set `result` to a compact answer (matching lines, a count, a summary) — not the raw payload. Keep intermediate data in sandbox variables, out of context.
diff --git i/src/acp_cmd.rs w/src/acp_cmd.rs
index 348e37c..e107c4e 100644
--- i/src/acp_cmd.rs
+++ w/src/acp_cmd.rs
@@ -1632,6 +1632,7 @@ fn build_remote_dispatch_hook(bridge_slot: BridgeSlot) -> RemoteToolHook {
                 return Some(RemoteToolResult {
                     content: reason,
                     is_error: true,
+                    transient: false,
                 });
             }
             bridge
@@ -1640,6 +1641,7 @@ fn build_remote_dispatch_hook(bridge_slot: BridgeSlot) -> RemoteToolHook {
                 .map(|outcome| RemoteToolResult {
                     content: outcome.content,
                     is_error: outcome.is_error,
+                    transient: outcome.transient,
                 })
         })
     })
diff --git i/src/agent.rs w/src/agent.rs
index e0ec176..1dec3d8 100644
--- i/src/agent.rs
+++ w/src/agent.rs
@@ -124,6 +124,7 @@ pub type ProviderNoticeHook = Box<dyn Fn(&str) + Send + Sync>;
 pub struct RemoteToolResult {
     pub content: String,
     pub is_error: bool,
+    pub transient: bool,
 }
 
 /// Dispatches a tool the opcode facade doesn't know. Consulted only when
@@ -1413,6 +1414,7 @@ async fn run_inner(
                             crate::observability::tool_kind(&name),
                         )
                     });
+                    let mut transient = false;
                     let (mut content, is_error, mut outcome) = if name == EXECUTE_SCRIPT_TOOL {
                         // execute_script shares the live session with its
                         // Starlark sandbox thread, so the loop hands it an
@@ -1646,7 +1648,10 @@ async fn run_inner(
                                     match served {
                                         // Remote MCP: the bridge emits the
                                         // `mcp.remote_tool` span with its server alias.
-                                        Some(r) => (r.content, r.is_error, None),
+                                        Some(r) => {
+                                            transient = r.transient;
+                                            (r.content, r.is_error, None)
+                                        }
                                         None => {
                                             // Emit a standalone span only when no
                                             // `tool_span` is open for this call
@@ -1825,9 +1830,11 @@ async fn run_inner(
                     // a repeated truncation placeholder is exactly the kind of
                     // no-progress signal the detector must see (vikunja #1197).
                     if loop_detector.is_some() {
-                        round_observations.push(crate::loop_detector::CallObservation::new(
+                        let mut observation = crate::loop_detector::CallObservation::new(
                             &name, &input, is_error, &content,
-                        ));
+                        );
+                        observation.transient = transient;
+                        round_observations.push(observation);
                     }
 
                     tool_results.push(ContentBlock::ToolResult {
@@ -3951,12 +3958,14 @@ mod tests {
                         return Some(RemoteToolResult {
                             content: format!("REMOTE_LARGE_SENTINEL\n{}", "l".repeat(60_000)),
                             is_error: false,
+                            transient: false,
                         });
                     }
                     name.strip_prefix("mcp__bench__medium_")
                         .map(|index| RemoteToolResult {
                             content: format!("MEDIUM_SENTINEL_{index}\n{}", "m".repeat(30_000)),
                             is_error: false,
+                            transient: false,
                         })
                 })
             })),
@@ -4887,6 +4896,7 @@ mod tests {
                     handled.then(|| RemoteToolResult {
                         content: serde_json::json!({"pid": 4242}).to_string(),
                         is_error: false,
+                        transient: false,
                     })
                 })
             })),
@@ -5067,6 +5077,7 @@ mod tests {
                     handled.then(|| RemoteToolResult {
                         content: "remote-output\n".repeat(200),
                         is_error: false,
+                        transient: false,
                     })
                 })
             })),
@@ -5213,6 +5224,163 @@ mod tests {
         }
     }
 
+    async fn exercise_remote_failure_budget(enabled: bool, transient: bool) {
+        let dir = tempfile::tempdir().unwrap();
+        let mut cfg = Config::default();
+        cfg.loop_detector.enabled = enabled;
+        let session = shared(Session::new(dir.path().to_path_buf(), Arc::new(cfg)));
+        let mut responses = Vec::new();
+        for round in 0..8 {
+            let mut response = tool_call_resp(
+                &format!("failure-{round}-a"),
+                "mcp__mock__failure",
+                json!({"query":"same"}),
+            );
+            response.content.push(ContentBlock::ToolCall {
+                id: format!("failure-{round}-b"),
+                name: "mcp__mock__failure".into(),
+                input: json!({"query":"same"}),
+            });
+            response.content.push(ContentBlock::ToolCall {
+                id: format!("lookup-{round}"),
+                name: "mcp__mock__lookup".into(),
+                input: json!({"round":round}),
+            });
+            responses.push(response);
+        }
+        responses.push(end_turn_resp());
+        let provider = BenchmarkCaptureProvider::new(responses);
+        let contexts = provider.contexts_handle();
+        let dispatched = Arc::new(Mutex::new(Vec::new()));
+        let observed = Arc::clone(&dispatched);
+        let config = AgentConfig {
+            error_resume_budget: Some(0),
+            remote_tool_dispatch: Some(Box::new(move |name: &str, _input: &Value| {
+                let name = name.to_owned();
+                let observed = Arc::clone(&observed);
+                Box::pin(async move {
+                    observed.lock().unwrap().push(name.clone());
+                    Some(RemoteToolResult {
+                        is_error: name.ends_with("failure"),
+                        content: if name.ends_with("failure") {
+                            "opaque identical error"
+                        } else {
+                            "found"
+                        }
+                        .into(),
+                        transient,
+                    })
+                })
+            })),
+            ..AgentConfig::default()
+        };
+        let result = run(
+            &provider,
+            session,
+            vec![Message::user("test failure budget")],
+            &config,
+        )
+        .await;
+        let should_stop = enabled && !transient;
+        let rounds = if should_stop { 6 } else { 8 };
+        assert_eq!(
+            contexts.lock().unwrap().len(),
+            if should_stop { 6 } else { 9 }
+        );
+        assert_eq!(
+            dispatched.lock().unwrap().len(),
+            rounds * 3,
+            "duplicates must still dispatch separately"
+        );
+        assert_eq!(
+            result.stop_reason,
+            if should_stop {
+                StopReason::Aborted
+            } else {
+                StopReason::EndTurn
+            }
+        );
+        if should_stop {
+            let reason = result.error_message.as_deref().unwrap();
+            assert!(reason.contains("mcp__mock__failure") && reason.contains("6 rounds"));
+            assert_eq!(
+                provider.responses.lock().unwrap().len(),
+                3,
+                "no seventh generation or retry"
+            );
+        }
+        let mut calls = Vec::new();
+        let mut results = Vec::new();
+        for message in &result.messages {
+            for block in &message.content {
+                match block {
+                    ContentBlock::ToolCall { id, .. } => calls.push(id.clone()),
+                    ContentBlock::ToolResult { tool_use_id, .. } => {
+                        results.push(tool_use_id.clone())
+                    }
+                    _ => {}
+                }
+            }
+        }
+        assert_eq!(calls.len(), rounds * 3);
+        assert_eq!(
+            calls, results,
+            "every call including duplicate reads retains its result ID"
+        );
+    }
+
+    #[tokio::test]
+    async fn remote_failure_budget_stops_before_next_generation_with_paired_results() {
+        exercise_remote_failure_budget(true, false).await;
+    }
+
+    #[tokio::test]
+    async fn remote_failure_budget_disabled_preserves_dispatch() {
+        exercise_remote_failure_budget(false, false).await;
+    }
+
+    #[tokio::test]
+    async fn remote_failure_budget_excludes_explicit_transient_outcomes() {
+        exercise_remote_failure_budget(true, true).await;
+    }
+
+    #[tokio::test]
+    async fn permission_failures_stop_but_budget_resets_on_next_session_turn() {
+        let dir = tempfile::tempdir().unwrap();
+        let responses = (0..12)
+            .map(|i| tool_call_resp(&format!("denied-{i}"), "mcp__mock__denied", json!({})))
+            .collect();
+        let provider = BenchmarkCaptureProvider::new(responses);
+        let contexts = provider.contexts_handle();
+        let config = AgentConfig {
+            remote_tool_dispatch: Some(Box::new(|_, _| {
+                Box::pin(async {
+                    Some(RemoteToolResult {
+                        content: "permission denied".into(),
+                        is_error: true,
+                        transient: false,
+                    })
+                })
+            })),
+            ..AgentConfig::default()
+        };
+        let mut session = AgentSession::new(Box::new(provider), session_in(dir.path()), config);
+        for _ in 0..2 {
+            let result = session.prompt("retry after checking permissions").await;
+            assert_eq!(result.stop_reason, StopReason::Aborted);
+            assert!(result
+                .error_message
+                .as_deref()
+                .unwrap()
+                .contains("6 rounds"));
+        }
+        assert_eq!(
+            contexts.lock().unwrap().len(),
+            12,
+            "each turn owns a fresh detector; no budget leak or auto-resume"
+        );
+    }
+
     #[tokio::test]
     async fn remote_tool_dispatch_serves_unknown_tool() {
         let dir = tempfile::tempdir().unwrap();
@@ -5228,6 +5396,7 @@ mod tests {
                     (name == "mcp__srv__echo").then(|| RemoteToolResult {
                         content: format!("echoed:{}", input["msg"].as_str().unwrap_or("")),
                         is_error: false,
+                        transient: false,
                     })
                 })
             })),
diff --git i/src/agent_mcp.rs w/src/agent_mcp.rs
index 1a6073c..96adc38 100644
--- i/src/agent_mcp.rs
+++ w/src/agent_mcp.rs
@@ -144,6 +144,7 @@ impl AgentMcp {
                     return Some(RemoteToolResult {
                         content: reason,
                         is_error: true,
+                        transient: false,
                     });
                 }
                 bridge
@@ -152,6 +153,7 @@ impl AgentMcp {
                     .map(|outcome| RemoteToolResult {
                         content: outcome.content,
                         is_error: outcome.is_error,
+                        transient: outcome.transient,
                     })
             })
         })
diff --git i/src/loop_detector.rs w/src/loop_detector.rs
index e1cf1e9..4d13073 100644
--- i/src/loop_detector.rs
+++ w/src/loop_detector.rs
@@ -21,6 +21,10 @@ use std::collections::hash_map::DefaultHasher;
 use std::collections::HashMap;
 use std::hash::{Hash, Hasher};
 
+const FAILURE_WARN_ROUNDS: u32 = 3;
+const FAILURE_STOP_ROUNDS: u32 = 6;
+const MAX_TRACKED_FAILURES: usize = 128;
+
 /// One executed tool call inside a model round, reduced to fingerprints.
 /// The tool-call id is deliberately excluded: providers mint a fresh id for
 /// every call, so including it would make every round look novel.
@@ -34,6 +38,8 @@ pub struct CallObservation {
     pub result_fp: u64,
     /// Whether the result was an error (repeated failures are a loop signal).
     pub is_error: bool,
+    /// Explicit transport timeout/failure, not inferred from result text.
+    pub transient: bool,
 }
 
 impl CallObservation {
@@ -43,17 +49,18 @@ impl CallObservation {
             call_fp: fingerprint_call(name, input),
             result_fp: fingerprint_result(is_error, result_content),
             is_error,
+            transient: false,
         }
     }
 }
 
-/// Hash of one tool call: the name plus its serialized arguments. serde_json
-/// serialization is deterministic for a given `Value`, which is sufficient —
-/// both occurrences being compared come from the same provider parse path.
+/// Hash a call with recursively sorted object keys; preserve array order.
 pub fn fingerprint_call(name: &str, input: &Value) -> u64 {
     let mut h = DefaultHasher::new();
     name.hash(&mut h);
-    input.to_string().hash(&mut h);
+    let mut canonical = input.clone();
+    canonical.sort_all_objects();
+    canonical.to_string().hash(&mut h);
     h.finish()
 }
 
@@ -93,6 +100,9 @@ pub struct LoopDetector {
     /// seen. Incremented at most once per round so a parallel batch of
     /// identical calls counts as one observation (one detector round).
     pair_rounds: HashMap<(u64, u64, bool), u32>,
+    /// Exact error pairs survive novelty and pruning until matching success.
+    /// Timestamp/request-id changes in error content deliberately do not aggregate.
+    failure_rounds: HashMap<(u64, u64), u32>,
     /// Rounds with zero novel `(call, result)` pairs, consecutively.
     consecutive_no_novelty: u32,
     /// Call-set fingerprint of the previous round.
@@ -119,6 +129,7 @@ impl LoopDetector {
             cfg,
             steer_sections,
             pair_rounds: HashMap::new(),
+            failure_rounds: HashMap::new(),
             consecutive_no_novelty: 0,
             last_call_set: None,
             steered_call_set: None,
@@ -148,6 +159,60 @@ impl LoopDetector {
         }
         self.stats.rounds_observed += 1;
 
+        // Matching successes reset only their own operation, never other tools.
+        // Any matching success is progress even in a mixed batch: this budget
+        // intentionally targets uninterrupted exact failures, not flaky success.
+        // The independent novelty guard still observes every mixed batch.
+        for o in round.iter().filter(|o| !o.is_error) {
+            self.failure_rounds
+                .retain(|(call, _), _| *call != o.call_fp);
+        }
+        let mut failures: Vec<_> = round
+            .iter()
+            .filter(|o| o.is_error && !o.transient)
+            .map(|o| (o.call_fp, o.result_fp))
+            .collect();
+        failures.sort_unstable();
+        failures.dedup();
+        let mut failure_steer = None;
+        // Count existing failures before admitting new keys: actionable repeat
+        // stops take precedence over state exhaustion, independent of hash order.
+        failures.sort_by_key(|pair| !self.failure_rounds.contains_key(pair));
+        for pair in failures {
+            if !self.failure_rounds.contains_key(&pair)
+                && self.failure_rounds.len() >= MAX_TRACKED_FAILURES
+            {
+                return RoundVerdict::Break(format!("exact failure budget exhausted: {MAX_TRACKED_FAILURES} distinct failed calls in this turn (tools: {}); stopping rather than evicting protection", summarize_tools(round)));
+            }
+            let count = self.failure_rounds.entry(pair).or_insert(0);
+            *count += 1;
+            let name = &round.iter().find(|o| o.call_fp == pair.0).unwrap().name;
+            if *count >= FAILURE_STOP_ROUNDS {
+                return RoundVerdict::Break(format!("exact failure budget: tool {name} returned the same error in {count} rounds. Stopping this turn; change the request or start a new turn."));
+            }
+            if *count == FAILURE_WARN_ROUNDS {
+                failure_steer = Some(format!("Tool {name} returned the same error in {FAILURE_WARN_ROUNDS} rounds. Do not repeat unchanged inputs; inspect the error or ask the user for clarification."));
+            }
+        }
+        // Always observe the legacy window, including the warning round.
+        // Prefer a legacy Break; otherwise emit at most one steer per round.
+        let legacy = self.observe_legacy(round);
+        match legacy {
+            RoundVerdict::Break(_) | RoundVerdict::Steer(_) => legacy,
+            RoundVerdict::Proceed => {
+                if let Some(text) = failure_steer {
+                    self.stats.steers_emitted += 1;
+                    self.steered_call_set = self.last_call_set;
+                    self.ignored_steers = 0;
+                    RoundVerdict::Steer(text)
+                } else {
+                    RoundVerdict::Proceed
+                }
+            }
+        }
+    }
+
+    fn observe_legacy(&mut self, round: &[CallObservation]) -> RoundVerdict {
         // A parallel batch aggregates into ONE detector round: each distinct
         // pair increments its round-count once, however many duplicates the
         // batch carried.
@@ -300,6 +365,278 @@ mod tests {
         vec![obs("read_file", &json!({"path": "a.rs"}), false, "content")]
     }
 
+    #[test]
+    fn recovered_linear_trace_stops_before_twenty_eighth_round() {
+        // Calls/errors from recovered history; successful lookup payloads redacted.
+        let trace: Vec<Vec<Value>> =
+            serde_json::from_str(include_str!("../tests/fixtures/linear_retry_loop.json")).unwrap();
+        assert_eq!(trace.len(), 28);
+        let mut d = detector();
+        let stopped = trace.iter().position(|round| {
+            let observations: Vec<_> = round
+                .iter()
+                .map(|call| {
+                    obs(
+                        call["name"].as_str().unwrap(),
+                        &call["input"],
+                        call["is_error"].as_bool().unwrap(),
+                        call["content"].as_str().unwrap(),
+                    )
+                })
+                .collect();
+            match d.observe_round(&observations) {
+                RoundVerdict::Break(text) => {
+                    assert!(
+                        text.contains("mcp__linear__list_initiatives") && text.contains("6 rounds"),
+                        "{text}"
+                    );
+                    true
+                }
+                _ => false,
+            }
+        });
+        assert_eq!(stopped, Some(18), "sixth exact-repeat occurs in round 19");
+    }
+
+    #[test]
+    fn exact_failures_survive_novel_rounds_and_pruning() {
+        let mut d = detector();
+        for i in 0..6 {
+            let failure = obs(
+                "mcp__linear__list_initiatives",
+                &json!({"customView":"/initiatives"}),
+                true,
+                "unknown custom view",
+            );
+            let verdict = d.observe_round(&[
+                failure.clone(),
+                failure,
+                obs("lookup", &json!({"query":i}), false, "found"),
+            ]);
+            if i == 5 {
+                assert!(
+                    matches!(verdict, RoundVerdict::Break(ref text) if text.contains("list_initiatives") && text.contains("6"))
+                );
+            } else {
+                assert!(!matches!(verdict, RoundVerdict::Break(_)));
+            }
+            d.on_context_pruned();
+        }
+    }
+
+    #[test]
+    fn exact_failure_success_resets_only_matching_call() {
+        let mut d = detector();
+        let failure = obs("remote", &json!({"query":"a"}), true, "error");
+        for i in 0..5 {
+            d.observe_round(&[failure.clone(), obs("novel", &json!({"i":i}), false, "ok")]);
+        }
+        d.observe_round(&[obs("remote", &json!({"query":"a"}), false, "ok")]);
+        assert!(!matches!(
+            d.observe_round(&[failure]),
+            RoundVerdict::Break(_)
+        ));
+    }
+
+    #[test]
+    fn exact_failure_changed_arguments_do_not_aggregate() {
+        let mut d = detector();
+        for i in 0..20 {
+            assert_eq!(
+                d.observe_round(&[obs("remote", &json!({"query":i}), true, "error")]),
+                RoundVerdict::Proceed
+            );
+        }
+    }
+
+    #[test]
+    fn exact_failure_transient_and_polling_do_not_accumulate() {
+        let mut d = detector();
+        for i in 0..12 {
+            let mut transient = obs("remote", &json!({}), true, "timeout");
+            transient.transient = true;
+            let verdict = d.observe_round(&[
+                transient,
+                obs("poll", &json!({}), false, "pending"),
+                obs("lookup", &json!({"i":i}), false, "ok"),
+            ]);
+            assert!(!matches!(verdict, RoundVerdict::Break(_)));
+        }
+        assert!(d.failure_rounds.is_empty());
+    }
+
+    #[test]
+    fn exact_failure_interleaved_tools_and_new_turn() {
+        let mut d = detector();
+        for i in 0..10 {
+            let name = if i % 2 == 0 { "a" } else { "b" };
+            assert!(!matches!(
+                d.observe_round(&[obs(
+                    name,
+                    &json!({}),
+                    true,
+                    "timeout text from opaque server"
+                )]),
+                RoundVerdict::Break(_)
+            ));
+        }
+        assert!(matches!(
+            d.observe_round(&[obs(
+                "a",
+                &json!({}),
+                true,
+                "timeout text from opaque server"
+            )]),
+            RoundVerdict::Break(_)
+        ));
+        assert_eq!(
+            detector().observe_round(&[obs(
+                "a",
+                &json!({}),
+                true,
+                "timeout text from opaque server"
+            )]),
+            RoundVerdict::Proceed
+        );
+    }
+
+    #[test]
+    fn exact_failure_changed_result_does_not_aggregate() {
+        let mut d = detector();
+        for i in 0..20 {
+            assert!(!matches!(
+                d.observe_round(&[obs("remote", &json!({}), true, &format!("error {i}"))]),
+                RoundVerdict::Break(_)
+            ));
+        }
+    }
+
+    #[test]
+    fn exact_failure_canonical_arguments_match() {
+        let a: Value = serde_json::from_str(r#"{"a":1,"b":{"c":2,"d":3}}"#).unwrap();
+        let b: Value = serde_json::from_str(r#"{"b":{"d":3,"c":2},"a":1}"#).unwrap();
+        assert_eq!(
+            fingerprint_call("remote", &a),
+            fingerprint_call("remote", &b)
+        );
+    }
+
+    #[test]
+    fn exact_failure_mixed_batch_resets_then_counts_failure_once() {
+        // Results are aggregated, not a guaranteed completion chronology:
+        // a matching success clears prior rounds, then batch failures count once.
+        for success_first in [true, false] {
+            let mut d = detector();
+            let failure = obs("remote", &json!({}), true, "error");
+            for i in 0..5 {
+                d.observe_round(&[failure.clone(), obs("lookup", &json!({"i":i}), false, "ok")]);
+            }
+            let success = obs("remote", &json!({}), false, "ok");
+            let batch = if success_first {
+                vec![success, failure.clone()]
+            } else {
+                vec![failure.clone(), success]
+            };
+            assert!(!matches!(d.observe_round(&batch), RoundVerdict::Break(_)));
+            assert_eq!(
+                d.failure_rounds.get(&(failure.call_fp, failure.result_fp)),
+                Some(&1)
+            );
+            for i in 0..4 {
+                assert!(!matches!(
+                    d.observe_round(&[
+                        failure.clone(),
+                        obs("lookup", &json!({"i":i+10}), false, "ok")
+                    ]),
+                    RoundVerdict::Break(_)
+                ));
+            }
+            assert!(matches!(
+                d.observe_round(&[failure]),
+                RoundVerdict::Break(_)
+            ));
+        }
+    }
+
+    #[test]
+    fn matching_success_each_round_keeps_exact_budget_reset() {
+        let mut d = detector();
+        for i in 0..20 {
+            assert!(!matches!(
+                d.observe_round(&[
+                    obs("remote", &json!({}), true, "error"),
+                    obs("remote", &json!({}), false, &format!("progress {i}")),
+                ]),
+                RoundVerdict::Break(_)
+            ));
+            assert_eq!(
+                d.failure_rounds.values().copied().collect::<Vec<_>>(),
+                vec![1]
+            );
+        }
+    }
+
+    #[test]
+    fn sixth_repeat_takes_precedence_over_state_cap() {
+        let mut d = detector();
+        let repeat = obs("remote", &json!({"repeat":true}), true, "error");
+        for i in 0..5 {
+            d.observe_round(&[repeat.clone(), obs("lookup", &json!({"i":i}), false, "ok")]);
+        }
+        for i in 0..127 {
+            d.observe_round(&[obs("remote", &json!({"unique":i}), true, "error")]);
+        }
+        assert!(
+            matches!(d.observe_round(&[obs("remote", &json!({"new":true}), true, "error"), repeat]), RoundVerdict::Break(text) if text.contains("6 rounds"))
+        );
+    }
+
+    #[test]
+    fn warning_round_updates_legacy_window_once() {
+        let mut d = detector();
+        let failure = obs("remote", &json!({}), true, "error");
+        d.observe_round(&[failure.clone()]);
+        d.observe_round(&[failure.clone()]);
+        assert!(matches!(
+            d.observe_round(&[failure]),
+            RoundVerdict::Steer(_)
+        ));
+        assert_eq!(d.consecutive_no_novelty, 2);
+        assert_eq!(d.pair_rounds.values().copied().collect::<Vec<_>>(), vec![3]);
+        assert_eq!(d.stats.steers_emitted, 1);
+        assert_eq!(d.steered_call_set, d.last_call_set);
+    }
+
+    #[test]
+    fn legacy_detector_treats_reordered_object_keys_as_same_call() {
+        // Intentional scope: canonicalization improves both detectors, not dispatch.
+        let mut d = detector();
+        for i in 0..3 {
+            let input: Value = serde_json::from_str(if i % 2 == 0 {
+                r#"{"a":1,"b":2}"#
+            } else {
+                r#"{"b":2,"a":1}"#
+            })
+            .unwrap();
+            let verdict = d.observe_round(&[obs("remote", &input, false, "pending")]);
+            assert_eq!(matches!(verdict, RoundVerdict::Steer(_)), i == 2);
+        }
+    }
+
+    #[test]
+    fn exact_failure_keys_are_bounded() {
+        let mut d = detector();
+        for i in 0..128 {
+            assert!(!matches!(
+                d.observe_round(&[obs("remote", &json!({"query":i}), true, "error")]),
+                RoundVerdict::Break(_)
+            ));
+        }
+        assert!(
+            matches!(d.observe_round(&[obs("remote", &json!({"query":128}), true, "error")]), RoundVerdict::Break(ref text) if text.contains("128"))
+        );
+    }
+
     #[test]
     fn identical_call_and_result_steers_at_threshold() {
         let mut d = detector();
diff --git i/src/mcp_bridge.rs w/src/mcp_bridge.rs
index 5e594fe..8852369 100644
--- i/src/mcp_bridge.rs
+++ w/src/mcp_bridge.rs
@@ -448,6 +448,8 @@ fn metadata_is_executable(_metadata: &std::fs::Metadata) -> bool {
 pub struct RemoteToolOutcome {
     pub content: String,
     pub is_error: bool,
+    /// Only explicit local timeout is classified; opaque SDK/server errors stay unknown.
+    pub transient: bool,
 }
 
 /// Maps an exposed (namespaced) tool name to the client that serves it and the
@@ -882,6 +884,7 @@ impl McpBridge {
                     RemoteToolOutcome {
                         content: format!("remote MCP tool '{name}' failed: {e}"),
                         is_error: true,
+                        transient: false,
                     },
                     crate::observability::ToolStatus::Error,
                 ),
@@ -892,6 +895,7 @@ impl McpBridge {
                             self.call_timeout.as_secs()
                         ),
                         is_error: true,
+                        transient: true,
                     },
                     crate::observability::ToolStatus::Timeout,
                 ),
@@ -1252,7 +1256,11 @@ fn result_to_outcome(result: CallToolResult) -> RemoteToolOutcome {
     } else {
         serde_json::to_string(&result.content).unwrap_or_default()
     };
-    RemoteToolOutcome { content, is_error }
+    RemoteToolOutcome {
+        content,
+        is_error,
+        transient: false,
+    }
 }
 
 /// No-op client handler: daimonos is a pure tool consumer, so it declines
@@ -1531,6 +1539,26 @@ mod tests {
         };
         let outcome = result_to_outcome(result);
         assert!(outcome.is_error);
+        assert!(
+            !outcome.transient,
+            "opaque server errors are not transport timeouts"
+        );
+    }
+
+    #[test]
+    fn opaque_timeout_text_is_not_transient() {
+        use rust_mcp_sdk::schema::TextContent;
+        let result = CallToolResult {
+            content: vec![ContentBlock::TextContent(TextContent::new(
+                "connection timed out".into(),
+                None,
+                None,
+            ))],
+            is_error: Some(true),
+            meta: None,
+            structured_content: None,
+        };
+        assert!(!result_to_outcome(result).transient);
     }
 
     // --- build: fail-open + disabled ---
diff --git i/src/prompts.rs w/src/prompts.rs
index ffcfc86..dfa63dd 100644
--- i/src/prompts.rs
+++ w/src/prompts.rs
@@ -408,6 +408,15 @@ mod tests {
         );
     }
 
+    #[test]
+    fn batching_prompts_exclude_remote_mcp_tools() {
+        for prompt in [AGENT_SYSTEM_DEFAULT, MCP_INSTRUCTIONS_DEFAULT] {
+            assert!(prompt.contains("native/local"));
+            assert!(prompt.contains("remote MCP"));
+            assert!(prompt.contains("direct"));
+        }
+    }
+
     #[test]
     fn agent_system_skips_routine_plans_and_coordination_overhead() {
         let prompt = AGENT_SYSTEM_DEFAULT.to_lowercase();
diff --git i/src/script.rs w/src/script.rs
index fb90d9d..e66cbb7 100644
--- i/src/script.rs
+++ w/src/script.rs
@@ -696,7 +696,8 @@ fn dispatch_request(request: Request, label: &str) -> Result<Response, anyhow::E
 
 /// Dispatch any native tool by name. Opcode-backed tools take the compact op
 /// path; plugin and meta tools fall through to the same shared dispatcher used
-/// by MCP and the agent loop. Keeping `tool("…")` universal prevents the model
+/// by MCP and the agent loop (not remote MCP integrations). Keeping native
+/// `tool("…")` universal prevents the model
 /// from spending retry turns learning which names require dedicated bindings.
 fn dispatch_tool_by_name(name: &str, args: &serde_json::Value) -> Result<Response, anyhow::Error> {
     let resp = match tools::build_request(name, args) {
@@ -710,6 +711,12 @@ fn dispatch_tool_by_name(name: &str, args: &serde_json::Value) -> Result<Respons
                 let Some((content, is_error, meta)) =
                     crate::mcp::dispatch_local_tool(&mut session, name, args).await
                 else {
+                    if name.starts_with("mcp__") {
+                        return Err(anyhow::anyhow!(
+                            "remote MCP tool '{name}' is unavailable in execute_script: \
+                             tool() supports native/local tools only; call this tool directly"
+                        ));
+                    }
                     return Err(anyhow::anyhow!("unknown tool '{name}'"));
                 };
 
@@ -1665,6 +1672,20 @@ mod tests {
         );
     }
 
+    #[tokio::test]
+    async fn remote_tool_in_script_explains_direct_call_boundary() {
+        let error = execute(
+            "result = tool(\"mcp__linear__get_user\", query=\"Patrick Clawson\")",
+            test_session(),
+            Duration::from_secs(5),
+        )
+        .await
+        .expect_err("remote integrations are not script bindings");
+        assert!(error.contains("mcp__linear__get_user"));
+        assert!(error.contains("native/local"));
+        assert!(error.contains("direct"));
+    }
+
     #[tokio::test]
     async fn execute_simple_expression() {
         let session = test_session();
diff --git 1/tests/fixtures/linear_retry_loop.json 2/tests/fixtures/linear_retry_loop.json
new file mode 100644
index 0000000..3081440
--- /dev/null
+++ 2/tests/fixtures/linear_retry_loop.json
@@ -0,0 +1,996 @@
+[
+  [
+    {
+      "name": "execute_script",
+      "input": {
+        "code": "def main():\n    return {\"user\": tool(\"mcp__linear__get_user\", query=\"Patrick Clawson\"), \"team\": tool(\"mcp__linear__get_team\", query=\"Infra\"), \"initiative\": tool(\"mcp__linear__get_initiative\", query=\"Infrastructure\", includeProjects=False, includeSubInitiatives=False)}\nresult = main()",
+        "timeout": 60
+      },
+      "is_error": true,
+      "content": "eval error: Traceback (most recent call last):\n  * script:3, in <module>\n      result = main()\n  * script:2, in main\n      return {\"user\": tool(\"mcp__linear__get_user\", query=\"Patrick Clawson\"), \"team...\nerror: unknown tool 'mcp__linear__get_user'\n --> script:2:21\n  |\n2 |     return {\"user\": tool(\"mcp__linear__get_user\", query=\"Patrick Clawson\"), \"team\": tool(\"mcp__linear__get_team\", query=\"Infra\"), \"initiative\": tool(\"mcp__linear__get_initiative\", query=\"Infrastructure\", includeProjects=False, includeSubInitiatives=False)}\n  |                     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^\n  |\n"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__get_user",
+      "input": {
+        "query": "Patrick Clawson"
+      },
+      "is_error": false,
+      "content": "successful lookup (redacted)"
+    },
+    {
+      "name": "mcp__linear__get_team",
+      "input": {
+        "query": "Infra"
+      },
+      "is_error": false,
+      "content": "successful lookup (redacted)"
+    },
+    {
+      "name": "mcp__linear__get_initiative",
+      "input": {
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "query": "Infrastructure"
+      },
+      "is_error": true,
+      "content": "Error: Could not find initiative \"Infrastructure\". The name matches more than one initiative, so pass its ID"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "Infrastructure",
+        "fields": [
+          "id",
+          "name",
+          "summary",
+          "url",
+          "leadTeam"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"Infrastructure\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": " ",
+        "fields": [
+          "id",
+          "name",
+          "summary",
+          "url",
+          "leadTeam"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Input validation error: Invalid arguments for tool list_initiatives: customView: Too small: expected string to have >=1 characters"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "all",
+        "fields": [
+          "id",
+          "name",
+          "summary",
+          "url",
+          "leadTeam"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"all\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/",
+        "fields": [
+          "id",
+          "name",
+          "summary",
+          "url",
+          "leadTeam"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "Infrastructure",
+        "fields": [
+          "id",
+          "name",
+          "summary",
+          "url",
+          "leadTeam"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "7c985836-2c13-4aa7-ac9e-bc84e519ad15",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"Infrastructure\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "00000000-0000-0000-0000-000000000000",
+        "fields": [
+          "id",
+          "name",
+          "summary",
+          "url",
+          "leadTeam"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"00000000-0000-0000-0000-000000000000\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "Infrastructure",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"Infrastructure\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "Infrastructure",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"Infrastructure\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "Infrastructure",
+        "fields": [
+          "id",
+          "name"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"Infrastructure\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "Infrastructure",
+        "fields": [
+          "id",
+          "name"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"Infrastructure\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "Infrastructure",
+        "fields": [
+          "id",
+          "name"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"Infrastructure\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    },
+    {
+      "name": "mcp__linear__list_custom_views",
+      "input": {
+        "cursor": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "query": "",
+        "shared": true,
+        "team": "",
+        "type": "Initiative"
+      },
+      "is_error": false,
+      "content": "successful lookup (redacted)"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    },
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    },
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    },
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    },
+    {
+      "name": "mcp__linear__get_workspace",
+      "input": {},
+      "is_error": false,
+      "content": "successful lookup (redacted)"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    },
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    },
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    },
+    {
+      "name": "mcp__linear__get_initiative",
+      "input": {
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "query": "https://linear.app/meetalix/initiative/infrastructure"
+      },
+      "is_error": true,
+      "content": "Error: Could not find initiative \"https://linear.app/meetalix/initiative/infrastructure\""
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ],
+  [
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    },
+    {
+      "name": "mcp__linear__list_initiatives",
+      "input": {
+        "createdAt": "",
+        "cursor": "",
+        "customView": "/initiatives",
+        "fields": [
+          "id",
+          "name",
+          "url"
+        ],
+        "includeArchived": false,
+        "includeProjects": false,
+        "includeSubInitiatives": false,
+        "label": "",
+        "leadTeam": "",
+        "limit": 50,
+        "orderBy": "updatedAt",
+        "owner": "",
+        "parentInitiative": "",
+        "query": "Infrastructure",
+        "status": "",
+        "updatedAt": ""
+      },
+      "is_error": true,
+      "content": "Error: Could not find custom view \"/initiatives\". Use list_custom_views to find a view, or pass its ID or URL for a project or initiative scoped view"
+    }
+  ]
+]

### New Points
None.

**Requesting:** RE-REVIEW
