# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1464-outlier-trace-report" -->

<!-- event id="request" artifact path="1464-outlier-trace-report/artifacts/round-1-review-request.diff" sha256="cc944705e2bb847ce15d374fa8570311f1674912bbec486fb8bd4301924c1406" -->
## Review Request — Round 1
**Task:** 1464 — Report trace-enabled SWE-bench outlier reruns
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Record the two full-50 Daimonos outliers and one trace-enabled rerun each as a separate SWO0/SWO1 diagnostic lineage. Preserve correctness regression, make no aggregate savings claim because rerun correctness is 1/2, summarize ordered tool traces, and recommend bounded diagnostics rather than task-specific tuning or another full run.

### Relevant Code / Diff
diff --git i/benchmarks/optimization-lineage.json w/benchmarks/optimization-lineage.json
index ea3fc9b..5eb24d7 100644
--- i/benchmarks/optimization-lineage.json
+++ w/benchmarks/optimization-lineage.json
@@ -1734,6 +1734,134 @@
           "kind": "comparison"
         }
       ]
+    },
+    {
+      "id": "SWO0",
+      "parent": null,
+      "feature": "Full-50 source attempts for dominant Daimonos outliers",
+      "scope_fingerprint": "swebench-outliers-8638-9229-v1|openrouter|anthropic/claude-opus-4.8|explicit-prompt-cache|official-docker",
+      "task_set_fingerprint": "sphinx-doc__sphinx-8638|sphinx-doc__sphinx-9229",
+      "benchmark_kind": "diagnostic",
+      "provider": "openrouter",
+      "model": "anthropic/claude-opus-4.8",
+      "thinking": "provider-default",
+      "task_ids": [
+        "sphinx-doc__sphinx-8638",
+        "sphinx-doc__sphinx-9229"
+      ],
+      "binary_sha256": "0c8843a01605111777737518e17161e6e65526802ad7a9d017dc37ecf017c316",
+      "git_commit": "780b272807b0df77cac796ae803dc677692b3698",
+      "fixture_commit": "6ce05e6b926c91faecdbb4243014ac85dca471dfad30ccdb69453343b75267c3",
+      "created_at": "2026-09-11T00:53:26Z",
+      "raw_run_dirs": [
+        "benchmarks/swebench/results/20260911-swebench-docker-openrouter-cache-full50-r1-merged"
+      ],
+      "metrics": {
+        "task_runs": [
+          2,
+          "count"
+        ],
+        "total_tokens": [
+          10091478,
+          "tokens"
+        ],
+        "llm_calls": [
+          155,
+          "count"
+        ],
+        "cost_usd": [
+          9.368654,
+          "usd"
+        ],
+        "wall_ms": [
+          1022266,
+          "milliseconds"
+        ],
+        "correct_task_runs": [
+          2,
+          "count"
+        ]
+      },
+      "notes": "Diagnostic baseline selected after the full-50 run because these two tasks contributed 34.79% of scoreable Daimonos cost.",
+      "report_path": "benchmarks/results/2026-09-11-swebench-outlier-tool-traces.md",
+      "artifacts": [
+        {
+          "path": "benchmarks/results/2026-09-11-swebench-outlier-tool-traces.md",
+          "sha256": "af20fe2714af72e2f56ee598975bf928be9c99935487626ea9993c029b221aaa",
+          "kind": "diagnostic"
+        }
+      ]
+    },
+    {
+      "id": "SWO1",
+      "parent": "SWO0",
+      "feature": "Trace-enabled reruns of dominant Daimonos outliers",
+      "scope_fingerprint": "swebench-outliers-8638-9229-v1|openrouter|anthropic/claude-opus-4.8|explicit-prompt-cache|official-docker",
+      "task_set_fingerprint": "sphinx-doc__sphinx-8638|sphinx-doc__sphinx-9229",
+      "benchmark_kind": "diagnostic",
+      "provider": "openrouter",
+      "model": "anthropic/claude-opus-4.8",
+      "thinking": "provider-default",
+      "task_ids": [
+        "sphinx-doc__sphinx-8638",
+        "sphinx-doc__sphinx-9229"
+      ],
+      "binary_sha256": "0c8843a01605111777737518e17161e6e65526802ad7a9d017dc37ecf017c316",
+      "git_commit": "113af7f83c765a8aeb72a947dd0ed4f39bed2fdc",
+      "fixture_commit": "6ce05e6b926c91faecdbb4243014ac85dca471dfad30ccdb69453343b75267c3",
+      "created_at": "2026-09-11T22:42:12Z",
+      "raw_run_dirs": [
+        "benchmarks/swebench/results/20260911-154212-swebench-docker-outlier-trace-8638",
+        "benchmarks/swebench/results/20260911-154722-swebench-docker-outlier-trace-9229"
+      ],
+      "metrics": {
+        "task_runs": [
+          2,
+          "count"
+        ],
+        "total_tokens": [
+          3821051,
+          "tokens"
+        ],
+        "llm_calls": [
+          77,
+          "count"
+        ],
+        "cost_usd": [
+          3.54932,
+          "usd"
+        ],
+        "wall_ms": [
+          579949,
+          "milliseconds"
+        ],
+        "correct_task_runs": [
+          1,
+          "count"
+        ],
+        "tool_trace_rows": [
+          92,
+          "count"
+        ]
+      },
+      "per_task_regressions": [
+        {
+          "task_id": "sphinx-doc__sphinx-9229",
+          "metric": "correct",
+          "baseline": 1,
+          "candidate": 0,
+          "delta_pct": -100
+        }
+      ],
+      "notes": "No aggregate savings claim: correctness regressed from 2/2 to 1/2. sphinx-8638 reproduced as a normal-cost successful run; sphinx-9229 remained long and failed after iterative debugging. Tool traces are private local SQLite artifacts.",
+      "report_path": "benchmarks/results/2026-09-11-swebench-outlier-tool-traces.md",
+      "artifacts": [
+        {
+          "path": "benchmarks/results/2026-09-11-swebench-outlier-tool-traces.md",
+          "sha256": "af20fe2714af72e2f56ee598975bf928be9c99935487626ea9993c029b221aaa",
+          "kind": "diagnostic"
+        }
+      ]
     }
   ]
 }
diff --git i/benchmarks/results/optimization-lineage.md w/benchmarks/results/optimization-lineage.md
index 9af31d3..ace1d75 100644
--- i/benchmarks/results/optimization-lineage.md
+++ w/benchmarks/results/optimization-lineage.md
@@ -235,3 +235,21 @@ per-task cost, token, call, wall-time, and correctness regression.
 
 Report:
 [`2026-09-11-swebench-openrouter-full50-r1.md`](2026-09-11-swebench-openrouter-full50-r1.md).
+
+## SWE-bench outlier diagnostic lineage
+
+Scope fingerprint: `swebench-outliers-8638-9229-v1 / OpenRouter /
+anthropic/claude-opus-4.8 / explicit prompt cache / official Docker`.
+
+| Stage | Parent | Run | Tokens | Calls | Cost | Wall | Correct | Delta |
+|---|---|---|---:|---:|---:|---:|---:|---|
+| SWO0 | — | Full-50 source attempts | 10,091,478 | 155 | $9.368654 | 1,022.266s | 2/2 | baseline |
+| SWO1 | SWO0 | Traced reruns | 3,821,051 | 77 | $3.549320 | 579.949s | 1/2 | not comparable: correctness regressed |
+
+No aggregate savings are claimed from SWO1. `sphinx-8638` preserved
+correctness while cost fell 79.57%; `sphinx-9229` cost fell 51.21% but failed.
+The traces show stochastic trajectory length and iterative debugging rather
+than a deterministic identical-tool loop.
+
+Report:
+[`2026-09-11-swebench-outlier-tool-traces.md`](2026-09-11-swebench-outlier-tool-traces.md).
diff --git i/benchmarks/swebench/README.md w/benchmarks/swebench/README.md
index b5052c2..55b23f6 100644
--- i/benchmarks/swebench/README.md
+++ w/benchmarks/swebench/README.md
@@ -100,6 +100,8 @@ OpenRouter cache experiment:
 [`2026-09-10-swebench-openrouter-cache-parity.md`](../results/2026-09-10-swebench-openrouter-cache-parity.md).
 Full-50 comparison:
 [`2026-09-11-swebench-openrouter-full50-r1.md`](../results/2026-09-11-swebench-openrouter-full50-r1.md).
+Outlier trace follow-up:
+[`2026-09-11-swebench-outlier-tool-traces.md`](../results/2026-09-11-swebench-outlier-tool-traces.md).
 
 Delete incomplete smoke directories created before dataset enrichment before
 treating `results/` as a baseline; a valid run contains `preds.jsonl`, a
diff --git 1/benchmarks/results/2026-09-11-swebench-outlier-tool-traces.md 2/benchmarks/results/2026-09-11-swebench-outlier-tool-traces.md
new file mode 100644
index 0000000..b1e35dc
--- /dev/null
+++ 2/benchmarks/results/2026-09-11-swebench-outlier-tool-traces.md
@@ -0,0 +1,95 @@
+# SWE-bench outlier reruns with tool traces
+
+## Scope
+
+Two dominant cached-Daimonos full-50 cost outliers were rerun once after
+per-instance SQLite tool-trace retention landed:
+
+- `sphinx-doc__sphinx-8638`
+- `sphinx-doc__sphinx-9229`
+
+The reruns used the same OpenRouter model
+`anthropic/claude-opus-4.8`, explicit prompt caching, official SWE-bench Docker
+images, compaction off, and musl binary
+`0c8843a01605111777737518e17161e6e65526802ad7a9d017dc37ecf017c316`.
+Only the Python benchmark runner changed: each instance now preserves the
+closed analytics database as a private standalone tool trace.
+
+## Results
+
+| Instance | Run | Tokens | Calls | Cost | Agent wall | Tool rows | Resolved |
+|---|---|---:|---:|---:|---:|---:|:---:|
+| `sphinx-8638` | full-50 | 4,551,795 | 71 | $3.602758 | 452.373s | unavailable | yes |
+| `sphinx-8638` | traced rerun | 582,623 | 24 | $0.735956 | 139.370s | 23 | yes |
+| `sphinx-9229` | full-50 | 5,539,683 | 84 | $5.765896 | 569.893s | unavailable | yes |
+| `sphinx-9229` | traced rerun | 3,238,428 | 53 | $2.813364 | 440.579s | 69 | **no** |
+
+`sphinx-8638` rerun deltas were tokens -87.20%, calls -66.20%, cost -79.57%,
+and wall -69.19%, with correctness preserved.
+
+`sphinx-9229` rerun deltas were tokens -41.54%, calls -36.90%, cost -51.21%,
+and wall -22.69%, but the patch failed. Those reductions are not
+correctness-gated savings.
+
+The two reruns cost $3.549320. Known attributable spend for the restoration,
+cache comparison, full-50 comparison, and these diagnostics is $54.914191,
+plus the unrecoverable first mini `sphinx-8548` attempt. The planning total
+with its $3 reserve is $57.914191, below the approved $70 cap. That reserve is
+not a provable bound because mini checks its threshold before the next request.
+
+## Trace findings
+
+### `sphinx-8638`
+
+The 23 rows show a normal bounded edit/test progression:
+
+- 8 searches;
+- 7 reads;
+- 3 edits;
+- 3 direct exec calls;
+- 2 git calls.
+
+There is no repeated identical-tool pattern or retry storm. The original
+71-call full-50 trajectory did not reproduce; this outlier is primarily
+stochastic based on current evidence.
+
+### `sphinx-9229`
+
+The 69 rows show a long iterative debugging workflow:
+
+- 15 reads and 5 searches;
+- 10 direct edits;
+- 14 script file writes;
+- 18 script exec calls, including 14 Python executions;
+- 3 direct exec calls and 2 script searches;
+- 2 git calls.
+
+Tool rows exceed the 53 model calls because `execute_script` records its nested
+operations individually. The sequence repeatedly writes/runs temporary Python
+checks and revises the implementation, but it is not an identical-call loop:
+the existing novelty-based loop detector would not be expected to trip.
+
+## Interpretation
+
+The full-50 cost concentration is not a stable fixed overhead:
+`sphinx-8638` returned to a normal-cost successful trajectory on rerun.
+`sphinx-9229` remained expensive and became incorrect despite fewer calls.
+
+The next change should not special-case these tasks or lower a global
+tool-repeat threshold. Better candidates are:
+
+1. retain richer privacy-safe argument/result fingerprints so novelty and
+   repeated debugging paths can be distinguished;
+2. add a configurable per-instance benchmark cost/wall guard so one stochastic
+   trajectory cannot dominate a run;
+3. investigate intra-run compaction for long one-shot tool loops.
+
+No second full-50 repetition is justified before one of those bounded
+diagnostic or safety changes lands.
+
+## Artifacts
+
+- `swebench/results/20260911-154212-swebench-docker-outlier-trace-8638`
+- `swebench/results/20260911-154722-swebench-docker-outlier-trace-9229`
+- `daimonos-anthropic__claude-opus-4.8.20260911-154212-swebench-docker-outlier-trace-8638.json`
+- `daimonos-anthropic__claude-opus-4.8.20260911-154722-swebench-docker-outlier-trace-9229.json`

### Known Concerns
1. Only one rerun per outlier exists, so stochasticity is suggested rather than proven.
2. Analytics anonymizes arguments and does not retain full results, limiting novelty diagnosis.
3. sphinx-9229 rerun used fewer resources but failed; no savings claim is valid.
4. One earlier mini operational cost remains unknown; cumulative spend uses a clearly labeled planning reserve.

### Specific Questions for Reviewer
1. Are before/after arithmetic and correctness treatment valid?
2. Do trace findings support the stated bounded conclusions without overclaiming?
3. Is SWO0/SWO1 correctly isolated from full-50 and five-task lineages?
4. Are all regressions and spend caveats retained?

<!-- event id="review-1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S1. Verified before/after arithmetic: sphinx-8638 deltas (tokens 4,551,795→582,623 = -87.20%; calls 71→24 = -66.20%; cost $3.602758→$0.735956 = -79.57%; wall 452.373s→139.370s = -69.19%) and sphinx-9229 deltas (tokens -41.54%; calls -36.90%; cost -51.21%; wall -22.69%) all check out, and SWO0/SWO1 aggregates (10,091,478 vs 3,821,051 tokens; 155 vs 77 calls; $9.368654 vs $3.549320; 2/2 vs 1/2 correct) match the per-instance rows. Refusing an aggregate savings claim given the 1/2 rerun correctness is the correct treatment; the report explicitly labels the sphinx-9229 reductions as not correctness-gated. Optionally, the lineage markdown could also note per-instance that sphinx-8638's -79.57% is a single-rerun observation, not an established improvement, to further guard against overreading n=1.
S2. The trace findings stay within bounded conclusions: 'primarily stochastic based on current evidence' for sphinx-8638 and 'not an identical-call loop' for sphinx-9229 are supported by the ordered tool-row summaries, and the known_concerns (single rerun per outlier, anonymized-argument analytics) are acknowledged in-text. The recommendation against task-specific tuning or a second full-50 run follows from that evidence. Consider adding a one-line explicit caveat in the Interpretation section that n=1 reruns cannot distinguish stochastic variance from bimodal behavior, mirroring the known_concerns list.
S3. SWO0/SWO1 isolation is correct: a distinct scope_fingerprint (swebench-outliers-8638-9229-v1), benchmark_kind 'diagnostic', a two-task task_set_fingerprint, and a null-parent SWO0 baseline cleanly separate this lineage from the full-50 and five-task chains, while raw_run_dirs correctly attribute SWO0 to the full-50 merged directory. The correctness regression is preserved in per_task_regressions (sphinx-9229 correct 1→0, -100%), and the spend caveats are retained: $54.914191 known attributable spend, the unrecoverable first mini attempt, the clearly labeled $3 planning reserve yielding $57.914191 under the $70 cap, and the note that the reserve is not a provable bound. No changes required.

**Verdict:** APPROVE
