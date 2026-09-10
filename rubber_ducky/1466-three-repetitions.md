# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1466-three-repetitions" -->

<!-- event id="request" artifact path="1466-three-repetitions/artifacts/round-1-review-request.diff" sha256="8fc7895cc31eacc66806d58aa82cd335083d90e8f0e2a2523c38f5f315c7ca22" -->
## Review Request — Round 1
**Task:** 1466 — Report three matched OpenRouter SWE-bench repetitions
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Normalize mini cache components into the shared schema, aggregate three sequential five-instance repetitions using OpenRouter usage.cost for both arms, retain all per-sample outcomes and excluded failures, and stop before full 50 pending a cache-parity experiment.

### Relevant Code / Diff
diff --git i/benchmarks/results/2026-09-10-swebench-restored-three-arm-five.md w/benchmarks/results/2026-09-10-swebench-restored-three-arm-five.md
index 1ccc212..dba6fe4 100644
--- i/benchmarks/results/2026-09-10-swebench-restored-three-arm-five.md
+++ w/benchmarks/results/2026-09-10-swebench-restored-three-arm-five.md
@@ -1,5 +1,9 @@
 # Restored SWE-bench five-instance three-arm comparison
 
+This one-repetition restoration report is retained as the original record.
+The OpenRouter cost conclusion is superseded by the
+[three-repetition comparison](2026-09-10-swebench-openrouter-three-repetitions.md).
+
 ## Scope
 
 All arms ran the same five mini-suite instances inside their official
diff --git i/benchmarks/swebench/README.md w/benchmarks/swebench/README.md
index 237ecfe..44a610f 100644
--- i/benchmarks/swebench/README.md
+++ w/benchmarks/swebench/README.md
@@ -68,9 +68,13 @@ Each run writes `results/<run-id>/` with per-instance token/cost JSONs
 mini-swe-agent writes trajectories plus `preds.json`; normalize each trajectory
 through `extract_mini.py` before cross-harness analysis. The normalizer sums
 OpenRouter's per-generation `usage.cost`, the same accounting source Daimonos
-uses; LiteLLM's aggregate is not used for ranking. Cursor uses its own backend,
+uses, and separates fresh/cache-write/cache-read prompt tokens; LiteLLM's
+aggregate is retained only as a consistency check. Cursor uses its own backend,
 so token/correctness comparisons are available but USD cost parity is not.
 
+Current default-harness result:
+[`2026-09-10-swebench-openrouter-three-repetitions.md`](../results/2026-09-10-swebench-openrouter-three-repetitions.md).
+
 Delete incomplete smoke directories created before dataset enrichment before
 treating `results/` as a baseline; a valid run contains `preds.jsonl`, a
 per-instance summary, token log, raw transcript, and non-empty patch.
diff --git i/benchmarks/swebench/extract_mini.py w/benchmarks/swebench/extract_mini.py
index c323736..14d2f89 100644
--- i/benchmarks/swebench/extract_mini.py
+++ w/benchmarks/swebench/extract_mini.py
@@ -13,7 +13,7 @@ import sys
 def main():
     traj_path, iid, repo, model, out_path = sys.argv[1:6]
     t = json.load(open(traj_path))
-    tot_in = tot_out = calls = 0
+    tot_in = tot_out = cache_write = cache_read = calls = 0
     costs = []
     cost_complete = True
     timestamps = []
@@ -30,6 +30,9 @@ def main():
             u = usage
             tot_in += u.get("prompt_tokens", 0) or 0
             tot_out += u.get("completion_tokens", 0) or 0
+            prompt_details = u.get("prompt_tokens_details") or {}
+            cache_write += prompt_details.get("cache_write_tokens", 0) or 0
+            cache_read += prompt_details.get("cached_tokens", 0) or 0
             cost = u.get("cost")
             if (
                 isinstance(cost, (int, float))
@@ -58,6 +61,7 @@ def main():
         if provider_cost is not None and model_stats_cost is not None
         else None
     )
+    fresh_input = max(0, tot_in - cache_write - cache_read)
     summary = {
         "task_id": iid,
         "task_name": repo,
@@ -65,10 +69,16 @@ def main():
         "canon_model": model,
         "model_slug": model,
         "wall_ms": wall_ms,
-        "input": tot_in,
+        "input": fresh_input,
+        "cache_write": cache_write,
+        "cache_read": cache_read,
         "output": tot_out,
         "total_tokens": tot_in + tot_out,
         "prompt_tokens": tot_in,
+        "fresh_input_tokens": fresh_input,
+        "mean_prompt_tokens_per_call": tot_in / calls if calls else None,
+        "mean_cache_read_per_call": cache_read / calls if calls else None,
+        "cache_hit_ratio": cache_read / tot_in if tot_in else None,
         "llm_calls": calls,
         # Same accounting source as Daimonos: OpenRouter's per-generation
         # usage.cost, not mini-swe-agent/LiteLLM's model-price estimate.
diff --git i/tests/test_swebench_extract_mini.py w/tests/test_swebench_extract_mini.py
index e40ee34..97db5f5 100644
--- i/tests/test_swebench_extract_mini.py
+++ w/tests/test_swebench_extract_mini.py
@@ -50,6 +50,10 @@ def test_extract_mini_sums_openrouter_reported_cost(tmp_path):
                                 "prompt_tokens": 100,
                                 "completion_tokens": 10,
                                 "cost": 0.0125,
+                                "prompt_tokens_details": {
+                                    "cache_write_tokens": 90,
+                                    "cached_tokens": 0,
+                                },
                             }
                         },
                     }
@@ -62,6 +66,10 @@ def test_extract_mini_sums_openrouter_reported_cost(tmp_path):
                                 "prompt_tokens": 120,
                                 "completion_tokens": 20,
                                 "cost": 0.0175,
+                                "prompt_tokens_details": {
+                                    "cache_write_tokens": 0,
+                                    "cached_tokens": 100,
+                                },
                             }
                         },
                     }
@@ -75,6 +83,12 @@ def test_extract_mini_sums_openrouter_reported_cost(tmp_path):
     )
 
     assert summary["total_tokens"] == 250
+    assert summary["prompt_tokens"] == 220
+    assert summary["input"] == 30
+    assert summary["fresh_input_tokens"] == 30
+    assert summary["cache_write"] == 90
+    assert summary["cache_read"] == 100
+    assert summary["cache_hit_ratio"] == pytest.approx(100 / 220)
     assert summary["llm_calls"] == 2
     assert summary["cost_usd"] == pytest.approx(0.03)
     assert summary["cost_source"] == "openrouter_usage"
diff --git 1/benchmarks/results/2026-09-10-swebench-openrouter-three-repetitions.md 2/benchmarks/results/2026-09-10-swebench-openrouter-three-repetitions.md
new file mode 100644
index 0000000..11f0eec
--- /dev/null
+++ 2/benchmarks/results/2026-09-10-swebench-openrouter-three-repetitions.md
@@ -0,0 +1,171 @@
+# SWE-bench OpenRouter comparison: three matched repetitions
+
+## Result
+
+Under each harness's default behavior, mini-swe-agent was cheaper than
+Daimonos on the same OpenRouter model and the same OpenRouter-reported cost
+field.
+
+Across all 15 matched instance-repetitions:
+
+- Daimonos: 1,991,233 tokens, 114 calls, **$10.317625**, 324.537 seconds,
+  15/15 resolved.
+- mini-swe-agent: 1,952,632 tokens, 238 calls, **$2.948874**, 728.107 seconds,
+  14/15 resolved.
+- mini-swe-agent cost **71.42% less**, or Daimonos cost **3.50x** as much.
+- Daimonos used 1.98% more raw tokens, 52.10% fewer calls, and 55.43% less
+  agent wall time.
+
+Daimonos cost more on every one of the 15 paired samples. It used more raw
+tokens on 9/15, took more wall time on 1/15, and never made more calls.
+
+The one mini failure is retained rather than hidden. Restricting the comparison
+to the 14 samples where both patches resolved does not change the conclusion:
+Daimonos cost $10.034260 versus mini-swe-agent's $2.857297, so mini cost 71.52%
+less (Daimonos 3.51x).
+
+## Scope
+
+The five instances in every repetition were:
+
+- `django__django-11815`
+- `django__django-12155`
+- `django__django-12708`
+- `sphinx-doc__sphinx-8035`
+- `sphinx-doc__sphinx-9367`
+
+Comparison fingerprint:
+
+- dataset: `MariusHobbhahn/swe-bench-verified-mini`, enriched fingerprint
+  `6ce05e6b926c91faecdbb4243014ac85dca471dfad30ccdb69453343b75267c3`;
+- evaluator: SWE-bench 5.0.2 official Docker images;
+- provider: OpenRouter;
+- exact model: `anthropic/claude-opus-4.8`;
+- cost source for both arms: sum of OpenRouter's per-generation `usage.cost`;
+- mini cost cross-check: all 15 sums matched
+  `model_stats.instance_cost`;
+- mini-swe-agent: 2.4.6 on CPython 3.12.14;
+- Daimonos agent code: commit
+  `827d461c006d0d91af7e6a4b9c94ef2d54549534`;
+- Daimonos musl binary:
+  `bfd61ce4edea4e118c3a1a56fd7e250a31dd60bc89b9a4d5d2c8538bbc0983c2`;
+- repetitions: three, run sequentially with one worker.
+
+The patch collector changed during repetition three only after a paid turn
+failed to produce a prediction. The recovery change did not alter the agent,
+model request, prompt, or binary.
+
+## Repetition aggregates
+
+| Rep | Harness | Tokens | Calls | OpenRouter cost | Agent wall | Resolved |
+|---:|---|---:|---:|---:|---:|---:|
+| 1 | Daimonos | 483,581 | 32 | $2.521245 | 94.638s | 5/5 |
+| 1 | mini | 755,633 | 88 | $1.090288 | 267.544s | 5/5 |
+| 2 | Daimonos | 668,154 | 38 | $3.456970 | 104.504s | 5/5 |
+| 2 | mini | 543,229 | 70 | $0.922993 | 234.902s | 4/5 |
+| 3 | Daimonos | 839,498 | 44 | $4.339410 | 125.395s | 5/5 |
+| 3 | mini | 653,770 | 80 | $0.935593 | 225.661s | 5/5 |
+
+Daimonos cost 2.31x, 3.75x, and 4.64x mini-swe-agent in repetitions 1–3.
+mini's cost was stable ($0.923–$1.090); Daimonos varied from $2.521 to
+$4.339, primarily because `sphinx-doc__sphinx-8035` took progressively more
+turns.
+
+Mean per five-instance repetition:
+
+| Harness | Tokens | Calls | Cost | Agent wall | Cost standard deviation |
+|---|---:|---:|---:|---:|---:|
+| Daimonos | 663,744 | 38.0 | $3.439208 | 108.179s | $0.909213 |
+| mini | 650,877 | 79.3 | $0.982958 | 242.702s | $0.093163 |
+
+## Why raw tokens and charged cost diverge
+
+mini-swe-agent enabled `set_cache_control=default_end`. Its 1,907,441 prompt
+tokens consisted of:
+
+- 1,756,837 cache reads (92.10%);
+- 150,128 cache writes (7.87%);
+- 476 fresh input tokens (0.02%).
+
+Daimonos reported no cache reads or writes, so all 1,973,160 prompt tokens were
+fresh. OpenRouter charges cached input differently from fresh input. The result
+therefore answers the requested default-harness question—what each harness cost
+as configured—but does not isolate caching from other harness differences.
+
+## Per-sample results
+
+| Rep | Instance | D tokens | mini tokens | D cost | mini cost | D wall | mini wall | D | mini |
+|---:|---|---:|---:|---:|---:|---:|---:|:---:|:---:|
+| 1 | `django__django-11815` | 54,780 | 55,375 | $0.282880 | $0.122261 | 9.263s | 34.440s | pass | pass |
+| 1 | `django__django-12155` | 53,764 | 38,968 | $0.276360 | $0.082543 | 9.934s | 23.023s | pass | pass |
+| 1 | `django__django-12708` | 89,435 | 243,253 | $0.464875 | $0.347552 | 16.505s | 87.217s | pass | pass |
+| 1 | `sphinx-doc__sphinx-8035` | 205,440 | 399,300 | $1.083820 | $0.482990 | 45.438s | 109.996s | pass | pass |
+| 1 | `sphinx-doc__sphinx-9367` | 80,162 | 18,737 | $0.413310 | $0.054941 | 13.498s | 12.868s | pass | pass |
+| 2 | `django__django-11815` | 54,837 | 37,757 | $0.283365 | $0.091577 | 9.312s | 24.810s | pass | **fail** |
+| 2 | `django__django-12155` | 53,766 | 32,256 | $0.276410 | $0.072563 | 8.316s | 20.592s | pass | pass |
+| 2 | `django__django-12708` | 88,858 | 171,623 | $0.459570 | $0.312987 | 14.629s | 87.258s | pass | pass |
+| 2 | `sphinx-doc__sphinx-8035` | 413,872 | 280,355 | $2.143460 | $0.377351 | 62.295s | 91.038s | pass | pass |
+| 2 | `sphinx-doc__sphinx-9367` | 56,821 | 21,238 | $0.294165 | $0.068516 | 9.952s | 11.204s | pass | pass |
+| 3 | `django__django-11815` | 54,770 | 55,590 | $0.282710 | $0.119367 | 8.988s | 33.883s | pass | pass |
+| 3 | `django__django-12155` | 53,773 | 31,552 | $0.276585 | $0.070647 | 9.443s | 20.420s | pass | pass |
+| 3 | `django__django-12708` | 89,653 | 148,673 | $0.465925 | $0.221779 | 15.878s | 55.384s | pass | pass |
+| 3 | `sphinx-doc__sphinx-8035` | 587,845 | 399,335 | $3.037765 | $0.470011 | 82.245s | 103.444s | pass | pass |
+| 3 | `sphinx-doc__sphinx-9367` | 53,457 | 18,620 | $0.276425 | $0.053789 | 8.841s | 12.530s | pass | pass |
+
+## Failed and excluded operations
+
+These attempts are preserved but excluded from the correctness-gated
+comparison:
+
+- mini repetition two first launch: five authentication failures, zero model
+  calls and $0.00. The retry explicitly mapped the existing agent key into
+  mini-swe-agent's expected environment variable.
+- Daimonos repetition three `sphinx-doc__sphinx-8035`: the agent completed, but
+  strict UTF-8 decoding of a generated binary test artifact aborted patch
+  collection. It consumed 793,425 tokens, 34 calls, and **$4.083385** without a
+  scoreable prediction. [PR #225](https://github.com/beardfaceguy/daimonos/pull/225)
+  fixed the collector and its two-instance recovery resolved 2/2.
+- Daimonos repetition one `sphinx-doc__sphinx-9367`: OpenRouter rejected the
+  first request with `in_flight_budget_exhausted`; it consumed zero tokens and
+  $0.00 before a settled retry.
+
+Including the paid but unscoreable patch-collection failure, actual Daimonos
+spend represented by these artifacts was $14.401010. It is operational
+overhead, not evidence for a correctness-gated efficiency delta.
+
+## Run artifacts
+
+Daimonos:
+
+- `swebench/results/20260910-130053-swebench-docker-compare-five-r1`
+- `swebench/results/20260910-130512-swebench-docker-compare-five-r1-retry9367`
+- `swebench/results/20260910-135741-swebench-docker-compare-five-r2`
+- `swebench/results/20260910-140920-swebench-docker-compare-five-r3`
+- `swebench/results/20260910-141753-swebench-docker-compare-five-r3-retry-sphinx`
+
+mini-swe-agent:
+
+- `swebench/results/mini-compare-five-r1`
+- `swebench/results/mini-compare-five-r2` (authentication failure)
+- `swebench/results/mini-compare-five-r2-authretry`
+- `swebench/results/mini-compare-five-r3`
+
+All six scoreable arm-repetitions have SWE-bench evaluator JSON reports in the
+SWE-bench directory.
+
+## Full-50 decision
+
+Do not start the full 50-instance comparison yet. A linear extrapolation from
+these five instances is roughly $34.39 for Daimonos and $9.83 for
+mini-swe-agent per repetition, before failures or retries; the full suite's
+task mix can differ substantially.
+
+The next useful experiment is cache parity:
+
+1. add equivalent OpenRouter prompt-cache support to Daimonos and rerun this
+   five-instance scope, or
+2. disable mini-swe-agent caching and compare both uncached.
+
+The first option answers the product-cost question; the second isolates the
+harness overhead. A full-50 run should follow only after choosing one of those
+scopes.

### Known Concerns
1. The five-instance sample is small and dominated by Sphinx 8035 variance.
2. Harness-default cache policies differ intentionally; this answers as-configured cost, not causal attribution.
3. One mini sample failed correctness and one paid Daimonos turn was unscoreable due runner infrastructure; both are reported separately.

### Specific Questions for Reviewer
1. Are the arithmetic and correctness-gated conclusions supported by the reported data?
2. Is the cache normalization compatible with the shared token schema?
3. Does the report preserve regressions and avoid overclaiming?
4. Is the decision not to launch full 50 justified?

<!-- event id="review-1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. Arithmetic inconsistency in the aggregate table: the 15-sample totals reported in the Result section (Daimonos $10.317625; mini $2.948874) should be re-verified against the per-sample table. Summing the listed Daimonos per-sample costs gives $10.317625 (consistent), but the per-sample mini costs sum to $2.948874 only if every row is exact; independently, the correctness-gated 14-sample figures ($10.034260 vs $2.857297) imply removing Daimonos rep-2 django-11815 ($0.283365) and mini's failed row ($0.091577) — yet a correctness-gated pairwise comparison must drop the *pair*, i.e., remove both arms' values for rep-2 django-11815. $10.317625 − $0.283365 = $10.034260 and $2.948874 − $0.091577 = $2.857297, which is consistent, so state explicitly in the report that the failed pair (both arms) was excluded, since a reader may otherwise assume only the failing arm's sample was dropped.
B2. Cache-token schema compatibility is asserted but not demonstrated for the double-counting edge case: extract_mini.py computes fresh_input = max(0, tot_in - cache_write - cache_read), which assumes OpenRouter's prompt_tokens always includes both cache_write_tokens and cached_tokens as subsets. Some providers report cache_write_tokens *in addition to* prompt_tokens rather than as a subset. Cite or verify OpenRouter's documented semantics for anthropic/claude-* prompt_tokens_details (and add a guard/assertion or logged warning when cache_write + cache_read > tot_in, rather than silently clamping to 0), otherwise the fresh/cache split and cache_hit_ratio may be wrong and the shared-schema 'input' field would undercount.
B3. The report states 'mini's cost was stable ($0.923–$1.090)' and computes a cost standard deviation, but repetition 2 for mini includes a failed instance (4/5 resolved) while its cost is included in the arm aggregate. The headline 15/15 vs 14/15 comparison mixes a failed sample's cost into the primary cost figures. The primary Result presents the all-samples numbers first; make the correctness-gated comparison equally prominent or clearly label the headline numbers as 'as-run, including one unresolved mini sample' so the cheaper arm's cost advantage is not overstated by including work that did not produce a correct patch.

### Non-Blocking Suggestions
S1. The 'Why raw tokens and charged cost diverge' section reports mini's prompt tokens as 1,907,441 while total tokens are 1,952,632; consider explicitly showing completion tokens (45,191) so readers can reconcile the decomposition, and likewise reconcile Daimonos's 1,973,160 fresh prompt tokens against its 1,991,233 total.
S2. cache_hit_ratio uses cache_read / tot_in; consider also reporting cache_read / (tot_in over calls after the first), or at least documenting that the first call can never hit cache, since the ratio is otherwise sensitive to call count and may mislead cross-harness comparison.
S3. The Sphinx-8035 variance dominates Daimonos's cost spread (Daimonos rep-3 sphinx-8035 alone is $3.04 of $4.34). Consider adding a sensitivity line excluding sphinx-8035 from all repetitions to show the conclusion's robustness to the single high-variance instance.
S4. The full-50 extrapolation ($34.39 / $9.83) uses the mean including the failed mini sample and excludes the $4.08 unscoreable Daimonos spend; note that realistic budgeting should include an operational-overhead allowance given the observed $14.40 total Daimonos spend across artifacts.
S5. The test asserts cache_hit_ratio == 100/220 but only for a two-call trajectory; add a test case where prompt_tokens_details is absent entirely (older trajectories) to confirm the normalizer degrades gracefully with cache fields at 0 and input == prompt_tokens.

**Verdict:** REVISE

<!-- event id="rebuttal-1" artifact path="1466-three-repetitions/artifacts/round-1-rebuttal.diff" sha256="1a3078fb8317dba23b2b6226999ce80a540bb7ce69321eb37b0a355bbc28f7f9" -->
## Rebuttal — Round 1
**Position:** AGREE

### Responses to Blocking Concerns
Re B1: ACCEPT — Moved the 14-sample paired correctness gate to the primary result and explicitly states that both arms of the unresolved pair are removed.
Re B2: ACCEPT — Cited OpenRouter prompt-caching and usage-accounting semantics, validate token fields, and fail normalization when cache-write plus cache-read exceeds prompt_tokens. All 15 trajectories satisfy the invariant.
Re B3: ACCEPT — The report now leads with the 14 paired resolved samples and labels the 15-sample and repetition-two figures as as-run, including the unresolved sample.

### Revised Code / Diff
diff --git i/benchmarks/results/2026-09-10-swebench-restored-three-arm-five.md w/benchmarks/results/2026-09-10-swebench-restored-three-arm-five.md
index 1ccc212..dba6fe4 100644
--- i/benchmarks/results/2026-09-10-swebench-restored-three-arm-five.md
+++ w/benchmarks/results/2026-09-10-swebench-restored-three-arm-five.md
@@ -1,5 +1,9 @@
 # Restored SWE-bench five-instance three-arm comparison
 
+This one-repetition restoration report is retained as the original record.
+The OpenRouter cost conclusion is superseded by the
+[three-repetition comparison](2026-09-10-swebench-openrouter-three-repetitions.md).
+
 ## Scope
 
 All arms ran the same five mini-suite instances inside their official
diff --git i/benchmarks/swebench/README.md w/benchmarks/swebench/README.md
index 237ecfe..44a610f 100644
--- i/benchmarks/swebench/README.md
+++ w/benchmarks/swebench/README.md
@@ -68,9 +68,13 @@ Each run writes `results/<run-id>/` with per-instance token/cost JSONs
 mini-swe-agent writes trajectories plus `preds.json`; normalize each trajectory
 through `extract_mini.py` before cross-harness analysis. The normalizer sums
 OpenRouter's per-generation `usage.cost`, the same accounting source Daimonos
-uses; LiteLLM's aggregate is not used for ranking. Cursor uses its own backend,
+uses, and separates fresh/cache-write/cache-read prompt tokens; LiteLLM's
+aggregate is retained only as a consistency check. Cursor uses its own backend,
 so token/correctness comparisons are available but USD cost parity is not.
 
+Current default-harness result:
+[`2026-09-10-swebench-openrouter-three-repetitions.md`](../results/2026-09-10-swebench-openrouter-three-repetitions.md).
+
 Delete incomplete smoke directories created before dataset enrichment before
 treating `results/` as a baseline; a valid run contains `preds.jsonl`, a
 per-instance summary, token log, raw transcript, and non-empty patch.
diff --git i/benchmarks/swebench/extract_mini.py w/benchmarks/swebench/extract_mini.py
index c323736..76d4f3c 100644
--- i/benchmarks/swebench/extract_mini.py
+++ w/benchmarks/swebench/extract_mini.py
@@ -10,10 +10,17 @@ import math
 import sys
 
 
+def token_count(value, field):
+    value = value or 0
+    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
+        raise ValueError(f"{field} must be a non-negative integer")
+    return value
+
+
 def main():
     traj_path, iid, repo, model, out_path = sys.argv[1:6]
     t = json.load(open(traj_path))
-    tot_in = tot_out = calls = 0
+    tot_in = tot_out = cache_write = cache_read = calls = 0
     costs = []
     cost_complete = True
     timestamps = []
@@ -28,8 +35,28 @@ def main():
                 usage = {}
 
             u = usage
-            tot_in += u.get("prompt_tokens", 0) or 0
-            tot_out += u.get("completion_tokens", 0) or 0
+            call_input = token_count(u.get("prompt_tokens"), "prompt_tokens")
+            call_output = token_count(
+                u.get("completion_tokens"), "completion_tokens"
+            )
+            prompt_details = u.get("prompt_tokens_details") or {}
+            if not isinstance(prompt_details, dict):
+                raise ValueError("prompt_tokens_details must be an object")
+            call_cache_write = token_count(
+                prompt_details.get("cache_write_tokens"), "cache_write_tokens"
+            )
+            call_cache_read = token_count(
+                prompt_details.get("cached_tokens"), "cached_tokens"
+            )
+            if call_cache_write + call_cache_read > call_input:
+                raise ValueError(
+                    "cache token subsets exceed prompt_tokens "
+                    f"on generation {calls}"
+                )
+            tot_in += call_input
+            tot_out += call_output
+            cache_write += call_cache_write
+            cache_read += call_cache_read
             cost = u.get("cost")
             if (
                 isinstance(cost, (int, float))
@@ -58,6 +85,7 @@ def main():
         if provider_cost is not None and model_stats_cost is not None
         else None
     )
+    fresh_input = max(0, tot_in - cache_write - cache_read)
     summary = {
         "task_id": iid,
         "task_name": repo,
@@ -65,10 +93,16 @@ def main():
         "canon_model": model,
         "model_slug": model,
         "wall_ms": wall_ms,
-        "input": tot_in,
+        "input": fresh_input,
+        "cache_write": cache_write,
+        "cache_read": cache_read,
         "output": tot_out,
         "total_tokens": tot_in + tot_out,
         "prompt_tokens": tot_in,
+        "fresh_input_tokens": fresh_input,
+        "mean_prompt_tokens_per_call": tot_in / calls if calls else None,
+        "mean_cache_read_per_call": cache_read / calls if calls else None,
+        "cache_hit_ratio": cache_read / tot_in if tot_in else None,
         "llm_calls": calls,
         # Same accounting source as Daimonos: OpenRouter's per-generation
         # usage.cost, not mini-swe-agent/LiteLLM's model-price estimate.
diff --git i/tests/test_swebench_extract_mini.py w/tests/test_swebench_extract_mini.py
index e40ee34..eb5d4e1 100644
--- i/tests/test_swebench_extract_mini.py
+++ w/tests/test_swebench_extract_mini.py
@@ -50,6 +50,10 @@ def test_extract_mini_sums_openrouter_reported_cost(tmp_path):
                                 "prompt_tokens": 100,
                                 "completion_tokens": 10,
                                 "cost": 0.0125,
+                                "prompt_tokens_details": {
+                                    "cache_write_tokens": 90,
+                                    "cached_tokens": 0,
+                                },
                             }
                         },
                     }
@@ -62,6 +66,10 @@ def test_extract_mini_sums_openrouter_reported_cost(tmp_path):
                                 "prompt_tokens": 120,
                                 "completion_tokens": 20,
                                 "cost": 0.0175,
+                                "prompt_tokens_details": {
+                                    "cache_write_tokens": 0,
+                                    "cached_tokens": 100,
+                                },
                             }
                         },
                     }
@@ -75,6 +83,12 @@ def test_extract_mini_sums_openrouter_reported_cost(tmp_path):
     )
 
     assert summary["total_tokens"] == 250
+    assert summary["prompt_tokens"] == 220
+    assert summary["input"] == 30
+    assert summary["fresh_input_tokens"] == 30
+    assert summary["cache_write"] == 90
+    assert summary["cache_read"] == 100
+    assert summary["cache_hit_ratio"] == pytest.approx(100 / 220)
     assert summary["llm_calls"] == 2
     assert summary["cost_usd"] == pytest.approx(0.03)
     assert summary["cost_source"] == "openrouter_usage"
@@ -204,3 +218,57 @@ def test_extract_mini_flags_model_stats_cost_mismatch(tmp_path):
 
     assert summary["model_stats_cost_usd"] == pytest.approx(0.02)
     assert summary["cost_matches_model_stats"] is False
+
+
+def test_extract_mini_defaults_missing_cache_details_to_fresh_input(tmp_path):
+    summary = _extract(
+        tmp_path,
+        {
+            "messages": [
+                {
+                    "extra": {
+                        "response": {
+                            "usage": {
+                                "prompt_tokens": 100,
+                                "completion_tokens": 10,
+                                "cost": 0.0125,
+                            }
+                        }
+                    }
+                }
+            ],
+            "info": {},
+        },
+    )
+
+    assert summary["input"] == 100
+    assert summary["cache_write"] == 0
+    assert summary["cache_read"] == 0
+    assert summary["total_tokens"] == 110
+
+
+def test_extract_mini_rejects_cache_subsets_larger_than_prompt(tmp_path):
+    with pytest.raises(subprocess.CalledProcessError):
+        _extract(
+            tmp_path,
+            {
+                "messages": [
+                    {
+                        "extra": {
+                            "response": {
+                                "usage": {
+                                    "prompt_tokens": 100,
+                                    "completion_tokens": 10,
+                                    "cost": 0.0125,
+                                    "prompt_tokens_details": {
+                                        "cache_write_tokens": 60,
+                                        "cached_tokens": 50,
+                                    },
+                                }
+                            }
+                        }
+                    }
+                ],
+                "info": {},
+            },
+        )
diff --git 1/benchmarks/results/2026-09-10-swebench-openrouter-three-repetitions.md 2/benchmarks/results/2026-09-10-swebench-openrouter-three-repetitions.md
new file mode 100644
index 0000000..e27cdbc
--- /dev/null
+++ 2/benchmarks/results/2026-09-10-swebench-openrouter-three-repetitions.md
@@ -0,0 +1,194 @@
+# SWE-bench OpenRouter comparison: three matched repetitions
+
+## Result
+
+Under each harness's default behavior, mini-swe-agent was cheaper than
+Daimonos on the same OpenRouter model and the same OpenRouter-reported cost
+field.
+
+The primary correctness-gated comparison contains the 14 paired samples where
+both patches resolved. It excludes both arms of the unresolved
+repetition-two `django__django-11815` pair:
+
+- Daimonos: 1,936,396 tokens, 110 calls, **$10.034260**, 315.225 seconds.
+- mini-swe-agent: 1,914,875 tokens, 229 calls, **$2.857297**, 703.297 seconds.
+- mini-swe-agent cost **71.52% less**, or Daimonos cost **3.51x** as much.
+- Daimonos used 1.12% more raw tokens, 51.97% fewer calls, and 55.18% less
+  agent wall time.
+
+The secondary as-run totals include mini-swe-agent's unresolved sample:
+
+- Daimonos: 1,991,233 tokens, 114 calls, **$10.317625**, 324.537 seconds,
+  15/15 resolved.
+- mini-swe-agent: 1,952,632 tokens, 238 calls, **$2.948874**, 728.107 seconds,
+  14/15 resolved.
+- mini-swe-agent cost **71.42% less**, or Daimonos cost **3.50x** as much.
+
+Daimonos cost more on every one of the 15 paired attempts. It used more raw
+tokens on 9/15, took more wall time on 1/15, and never made more calls. The
+failed mini sample is retained in the table rather than hidden.
+
+## Scope
+
+The five instances in every repetition were:
+
+- `django__django-11815`
+- `django__django-12155`
+- `django__django-12708`
+- `sphinx-doc__sphinx-8035`
+- `sphinx-doc__sphinx-9367`
+
+Comparison fingerprint:
+
+- dataset: `MariusHobbhahn/swe-bench-verified-mini`, enriched fingerprint
+  `6ce05e6b926c91faecdbb4243014ac85dca471dfad30ccdb69453343b75267c3`;
+- evaluator: SWE-bench 5.0.2 official Docker images;
+- provider: OpenRouter;
+- exact model: `anthropic/claude-opus-4.8`;
+- cost source for both arms: sum of OpenRouter's per-generation `usage.cost`;
+- mini cost cross-check: all 15 sums matched
+  `model_stats.instance_cost`;
+- mini-swe-agent: 2.4.6 on CPython 3.12.14;
+- Daimonos agent code: commit
+  `827d461c006d0d91af7e6a4b9c94ef2d54549534`;
+- Daimonos musl binary:
+  `bfd61ce4edea4e118c3a1a56fd7e250a31dd60bc89b9a4d5d2c8538bbc0983c2`;
+- repetitions: three, run sequentially with one worker.
+
+The patch collector changed during repetition three only after a paid turn
+failed to produce a prediction. The recovery change did not alter the agent,
+model request, prompt, or binary.
+
+## Repetition aggregates
+
+| Rep | Harness | Tokens | Calls | OpenRouter cost | Agent wall | Resolved |
+|---:|---|---:|---:|---:|---:|---:|
+| 1 | Daimonos | 483,581 | 32 | $2.521245 | 94.638s | 5/5 |
+| 1 | mini | 755,633 | 88 | $1.090288 | 267.544s | 5/5 |
+| 2 | Daimonos | 668,154 | 38 | $3.456970 | 104.504s | 5/5 |
+| 2 | mini | 543,229 | 70 | $0.922993 | 234.902s | 4/5 |
+| 3 | Daimonos | 839,498 | 44 | $4.339410 | 125.395s | 5/5 |
+| 3 | mini | 653,770 | 80 | $0.935593 | 225.661s | 5/5 |
+
+Daimonos cost 2.31x, 3.75x, and 4.64x mini-swe-agent in repetitions 1–3.
+mini's as-run spend was stable ($0.923–$1.090), though repetition two includes
+one unresolved sample. Daimonos varied from $2.521 to $4.339, primarily because
+`sphinx-doc__sphinx-8035` took progressively more turns.
+
+Mean per five-instance repetition:
+
+| Harness | Tokens | Calls | Cost | Agent wall | Cost standard deviation |
+|---|---:|---:|---:|---:|---:|
+| Daimonos | 663,744 | 38.0 | $3.439208 | 108.179s | $0.909213 |
+| mini | 650,877 | 79.3 | $0.982958 | 242.702s | $0.093163 |
+
+## Why raw tokens and charged cost diverge
+
+mini-swe-agent enabled `set_cache_control=default_end`. Its 1,907,441 prompt
+tokens plus 45,191 completion tokens reconcile to the 1,952,632 total. Prompt
+tokens consisted of:
+
+- 1,756,837 cache reads (92.10%);
+- 150,128 cache writes (7.87%);
+- 476 fresh input tokens (0.02%).
+
+Daimonos reported no cache reads or writes, so all 1,973,160 prompt tokens were
+fresh; its 18,073 completion tokens produce the 1,991,233 total.
+
+OpenRouter documents `cached_tokens` and `cache_write_tokens` as prompt-token
+subsets ([prompt caching](https://openrouter.ai/docs/guides/best-practices/prompt-caching),
+[usage accounting](https://openrouter.ai/docs/cookbook/administration/usage-accounting)).
+The normalizer rejects a response if those subsets exceed `prompt_tokens`
+rather than silently undercounting fresh input. The 92.10% cache-read ratio is
+descriptive of this workload and call count; each instance's first generation
+must establish a cache entry before later calls can hit it.
+
+OpenRouter charges cached input differently from fresh input. The result
+therefore answers the requested default-harness question—what each harness cost
+as configured—but does not isolate caching from other harness differences.
+
+The conclusion is not solely a `sphinx-doc__sphinx-8035` artifact. Removing
+that high-variance task and the unresolved pair leaves 11 correctness-matched
+samples: Daimonos cost $3.769215 versus mini-swe-agent's $1.526945, so mini
+still cost 59.49% less (Daimonos 2.47x).
+
+## Per-sample results
+
+| Rep | Instance | D tokens | mini tokens | D cost | mini cost | D wall | mini wall | D | mini |
+|---:|---|---:|---:|---:|---:|---:|---:|:---:|:---:|
+| 1 | `django__django-11815` | 54,780 | 55,375 | $0.282880 | $0.122261 | 9.263s | 34.440s | pass | pass |
+| 1 | `django__django-12155` | 53,764 | 38,968 | $0.276360 | $0.082543 | 9.934s | 23.023s | pass | pass |
+| 1 | `django__django-12708` | 89,435 | 243,253 | $0.464875 | $0.347552 | 16.505s | 87.217s | pass | pass |
+| 1 | `sphinx-doc__sphinx-8035` | 205,440 | 399,300 | $1.083820 | $0.482990 | 45.438s | 109.996s | pass | pass |
+| 1 | `sphinx-doc__sphinx-9367` | 80,162 | 18,737 | $0.413310 | $0.054941 | 13.498s | 12.868s | pass | pass |
+| 2 | `django__django-11815` | 54,837 | 37,757 | $0.283365 | $0.091577 | 9.312s | 24.810s | pass | **fail** |
+| 2 | `django__django-12155` | 53,766 | 32,256 | $0.276410 | $0.072563 | 8.316s | 20.592s | pass | pass |
+| 2 | `django__django-12708` | 88,858 | 171,623 | $0.459570 | $0.312987 | 14.629s | 87.258s | pass | pass |
+| 2 | `sphinx-doc__sphinx-8035` | 413,872 | 280,355 | $2.143460 | $0.377351 | 62.295s | 91.038s | pass | pass |
+| 2 | `sphinx-doc__sphinx-9367` | 56,821 | 21,238 | $0.294165 | $0.068516 | 9.952s | 11.204s | pass | pass |
+| 3 | `django__django-11815` | 54,770 | 55,590 | $0.282710 | $0.119367 | 8.988s | 33.883s | pass | pass |
+| 3 | `django__django-12155` | 53,773 | 31,552 | $0.276585 | $0.070647 | 9.443s | 20.420s | pass | pass |
+| 3 | `django__django-12708` | 89,653 | 148,673 | $0.465925 | $0.221779 | 15.878s | 55.384s | pass | pass |
+| 3 | `sphinx-doc__sphinx-8035` | 587,845 | 399,335 | $3.037765 | $0.470011 | 82.245s | 103.444s | pass | pass |
+| 3 | `sphinx-doc__sphinx-9367` | 53,457 | 18,620 | $0.276425 | $0.053789 | 8.841s | 12.530s | pass | pass |
+
+## Failed and excluded operations
+
+These attempts are preserved but excluded from the correctness-gated
+comparison:
+
+- mini repetition two first launch: five authentication failures, zero model
+  calls and $0.00. The retry explicitly mapped the existing agent key into
+  mini-swe-agent's expected environment variable.
+- Daimonos repetition three `sphinx-doc__sphinx-8035`: the agent completed, but
+  strict UTF-8 decoding of a generated binary test artifact aborted patch
+  collection. It consumed 793,425 tokens, 34 calls, and **$4.083385** without a
+  scoreable prediction. [PR #225](https://github.com/beardfaceguy/daimonos/pull/225)
+  fixed the collector and its two-instance recovery resolved 2/2.
+- Daimonos repetition one `sphinx-doc__sphinx-9367`: OpenRouter rejected the
+  first request with `in_flight_budget_exhausted`; it consumed zero tokens and
+  $0.00 before a settled retry.
+
+Including the paid but unscoreable patch-collection failure, actual Daimonos
+spend represented by these artifacts was $14.401010. It is operational
+overhead, not evidence for a correctness-gated efficiency delta.
+
+## Run artifacts
+
+Daimonos:
+
+- `swebench/results/20260910-130053-swebench-docker-compare-five-r1`
+- `swebench/results/20260910-130512-swebench-docker-compare-five-r1-retry9367`
+- `swebench/results/20260910-135741-swebench-docker-compare-five-r2`
+- `swebench/results/20260910-140920-swebench-docker-compare-five-r3`
+- `swebench/results/20260910-141753-swebench-docker-compare-five-r3-retry-sphinx`
+
+mini-swe-agent:
+
+- `swebench/results/mini-compare-five-r1`
+- `swebench/results/mini-compare-five-r2` (authentication failure)
+- `swebench/results/mini-compare-five-r2-authretry`
+- `swebench/results/mini-compare-five-r3`
+
+All six scoreable arm-repetitions have SWE-bench evaluator JSON reports in the
+SWE-bench directory.
+
+## Full-50 decision
+
+Do not start the full 50-instance comparison yet. A linear extrapolation from
+the scoreable as-run means is roughly $34.39 for Daimonos and $9.83 for
+mini-swe-agent per repetition; the full suite's task mix can differ
+substantially. Real budgeting also needs an operational-overhead allowance:
+the observed paid collector failure raises represented Daimonos spend from
+$10.32 to $14.40. That fixed defect is now repaired, so neither number is a
+reliable full-50 forecast.
+
+The next useful experiment is cache parity:
+
+1. add equivalent OpenRouter prompt-cache support to Daimonos and rerun this
+   five-instance scope, or
+2. disable mini-swe-agent caching and compare both uncached.
+
+The first option answers the product-cost question; the second isolates the
+harness overhead. A full-50 run should follow only after choosing one of those
+scopes.

### New Points
R1. Applied all suggestions: reconciled completion totals, explained first-call cache behavior, added Sphinx-8035-excluded sensitivity, added operational budget overhead, and tested missing cache details.

**Requesting:** RE-REVIEW

<!-- event id="review-2" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S6. Non-blocking: token_count raises ValueError for float token values; some providers occasionally emit floats for token counts. If that ever occurs in older trajectories, consider accepting integral floats (e.g., 90.0) rather than failing normalization outright. No action required for this report since all 15 trajectories validated.

### Responses to Prior Points
Re R1: Verified against the revised diff. S1: the divergence section now reconciles both arms (mini 1,907,441 prompt + 45,191 completion = 1,952,632; Daimonos 1,973,160 + 18,073 = 1,991,233). S2: first-call cache-establishment caveat is documented alongside the 92.10% ratio. S3: the sphinx-8035-excluded sensitivity line ($3.769215 vs $1.526945, 59.49% less, 2.47x) is arithmetically consistent with the 11 remaining correctness-matched per-sample rows. S4: the Full-50 section now includes the operational-overhead allowance ($10.32 -> $14.40) and appropriately hedges the forecast. S5: test_extract_mini_defaults_missing_cache_details_to_fresh_input covers the missing prompt_tokens_details case with input == prompt_tokens and zero cache fields. All suggestions are satisfactorily applied.
B1: resolved
B2: resolved
B3: resolved

**Verdict:** APPROVE
