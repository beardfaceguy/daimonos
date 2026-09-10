# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1450-comparison-restoration" -->

<!-- event id="request" artifact path="1450-comparison-restoration/artifacts/round-1-review-request.diff" sha256="7dada0b9056aa97743d878b9061254ad2b4fb8bef75f5a706453edc0a10a0270" -->
## Review Request — Round 1
**Task:** 1450 — Document restored comparison arms and matched five-instance results
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Update SWE-bench setup instructions for pinned mini-swe-agent and Cursor arms, record the local sphinx-8035 trajectory analysis, preserve three post-#1463 django repetitions, and report one correctness-gated five-instance three-arm comparison with explicit cache, cost, retry, and repetition caveats.

### Relevant Code / Diff
diff --git i/benchmarks/swebench/README.md w/benchmarks/swebench/README.md
index d3d228c..54ad763 100644
--- i/benchmarks/swebench/README.md
+++ w/benchmarks/swebench/README.md
@@ -15,7 +15,8 @@ FAIL_TO_PASS / PASS_TO_PASS tests in a per-instance Docker image.
 
 ```sh
 uv venv --python 3.12 .venv
-uv pip install --python .venv/bin/python 'swebench==5.0.2'
+uv pip install --python .venv/bin/python \
+  'swebench==5.0.2' 'mini-swe-agent==2.4.6'
 .venv/bin/python fetch_dataset.py      # writes instances.jsonl (50 rows)
 ```
 
@@ -49,11 +50,25 @@ Model/provider/key come from `~/.config/daimonos/agent.env`, exactly like
 
 # Full 50-instance run:
 .venv/bin/python run_agent.py --docker --tag <label>
+
+# Cursor arm (requires cursor-agent login):
+.venv/bin/python run_cursor.py --docker \
+  --model claude-opus-4-8-medium --tag <label>
+
+# mini-swe-agent arm (expects OPENROUTER_API_KEY in the environment):
+OPENROUTER_API_KEY=... .venv/bin/mini-extra swebench \
+  --subset MariusHobbhahn/swe-bench-verified-mini --split test \
+  --model openrouter/anthropic/claude-opus-4.8 \
+  --environment-class docker --workers 1 --output results/mini-<label>
 ```
 
 Each run writes `results/<run-id>/` with per-instance token/cost JSONs
 (same schema as the in-house suite — `../analyze.py results/` works),
 `.patch` files, raw transcripts, and `preds.jsonl`.
+mini-swe-agent writes trajectories plus `preds.json`; normalize each trajectory
+through `extract_mini.py` before cross-harness token analysis. Cursor uses its
+own backend, so token/correctness comparisons are available but USD cost parity
+is not.
 
 Delete incomplete smoke directories created before dataset enrichment before
 treating `results/` as a baseline; a valid run contains `preds.jsonl`, a
diff --git 1/benchmarks/results/2026-09-10-swebench-sphinx-8035-analysis.md 2/benchmarks/results/2026-09-10-swebench-sphinx-8035-analysis.md
new file mode 100644
index 0000000..78b888f
--- /dev/null
+++ 2/benchmarks/results/2026-09-10-swebench-sphinx-8035-analysis.md
@@ -0,0 +1,53 @@
+# SWE-bench `sphinx-8035` trajectory analysis
+
+## Finding
+
+The R1/R2 token spread is model-strategy variance, not retry or evaluator
+failure:
+
+| Metric | R1 | R2 | Change |
+|---|---:|---:|---:|
+| Total tokens | 381,756 | 521,539 | +36.62% |
+| LLM calls | 16 | 25 | +9 |
+| Tool-loop calls | 15 | 24 | +9 |
+| Cost | $1.977600 | $2.695695 | +36.31% |
+| Agent wall | 58.109 s | 79.222 s | +36.33% |
+| Final context estimate | 20,755 tokens | 16,013 tokens | -22.85% |
+| Patch size | 3,922 B | 4,446 B | +13.36% |
+| Correctness | resolved | resolved | unchanged |
+
+Every provider call succeeded. Neither run recorded a failed generation or tool
+result error. R1 used one two-operation `execute_script`; R2 used two, totaling
+four operations. The additional nine R2 calls therefore represent a longer
+successful exploration/verification path, not retries or a stuck identical-call
+loop.
+
+Prompt input dominates both runs: 378,315 of 381,756 tokens in R1 and 517,139
+of 521,539 in R2. Each extra generation re-sends fixed system and tool-schema
+context. R2's context was smaller per call and at the final call, but 25 calls
+overwhelmed that per-call reduction.
+
+Both patches implement the same core change: parse `private-members` with
+`members_option` and filter explicit private-member names. R2 additionally
+updates `CHANGES` and uses slightly different branching style. Both pass the
+official evaluator, so the extra R2 work did not change measured correctness.
+
+## Observability gap
+
+The Docker runner preserves token summaries, final text, stderr, and patches,
+but not Daimonos's per-tool analytics database. Exact tool names and arguments
+for the 15/24 tool loops are therefore unavailable after the container is
+removed. Context-component deltas can distinguish successful tool-loop growth
+from retries, but cannot identify which reads/searches/tests were repeated.
+
+Before optimizing this trajectory, the runner should mount a private per-task
+analytics directory and export an ordered, content-safe tool-call trace. That
+would separate useful exploration from redundant reads without retaining source
+or tool-result contents.
+
+## Conclusion
+
+Do not optimize from aggregate R1/R2 token totals alone. The actionable signal
+is call-count variance on `sphinx-8035`; the missing tool trace is the next
+measurement gap. Any intervention needs a matching baseline with repeated runs
+and must preserve this instance's per-run regressions.
diff --git 1/benchmarks/results/2026-09-10-post-1463-django-11815.md 2/benchmarks/results/2026-09-10-post-1463-django-11815.md
new file mode 100644
index 0000000..21615fc
--- /dev/null
+++ 2/benchmarks/results/2026-09-10-post-1463-django-11815.md
@@ -0,0 +1,42 @@
+# Post-#1463 `django-11815` repetitions
+
+## Scope
+
+- Purpose: verify the merged malformed ToolUse/assistant-prefill guard
+- Daimonos commit: `827d461c006d0d91af7e6a4b9c94ef2d54549534`
+- musl binary SHA-256:
+  `bfd61ce4edea4e118c3a1a56fd7e250a31dd60bc89b9a4d5d2c8538bbc0983c2`
+- Provider/model: OpenRouter, `anthropic/claude-opus-4.8`
+- Instance: `django__django-11815`
+- Docker image:
+  `swebench/sweb.eval.x86_64.django_1776_django-11815:latest`
+- Compaction: off
+- Repetitions: three
+
+## Results
+
+| Run | Tokens | LLM calls | Cost | Agent wall | Patch | Resolved |
+|---|---:|---:|---:|---:|---:|---:|
+| `post1463-r1` | 54,792 | 4 | $0.283260 | 10.209 s | 775 B | yes |
+| `post1463-r2` | 54,760 | 4 | $0.283140 | 9.170 s | 775 B | yes |
+| `post1463-r3` | 54,757 | 4 | $0.283065 | 9.113 s | 775 B | yes |
+| **Mean** | **54,769.7** | **4.0** | **$0.283155** | **9.497 s** | **775 B** | **3/3** |
+
+Token spread was 35 (0.064% of the mean), cost spread was $0.000195, and
+call count and patch size were identical. All three patches have SHA-256
+`e83dc1c9a5ddd5ba3dcc7c9b6ddfa4f60cc30d1d26924d849cfca561f8223b3b`.
+
+All evaluator runs completed without infrastructure failures, ambiguous
+failures, empty patches, evaluator errors, or leaked containers.
+
+## Interpretation
+
+The malformed provider shape did not recur in these three samples, so this is
+not a live positive-control exercise of the guard. It does show that the merged
+validation did not regress the ordinary successful tool-call path: all runs
+used four calls, produced byte-identical patches, and resolved the task.
+
+The captured regression tests remain the positive control for #1463: a
+ToolUse response without a structured ToolCall terminates before another
+provider request, while usage/cost and unrelated response-history semantics are
+preserved.
diff --git 1/benchmarks/results/2026-09-10-swebench-restored-three-arm-five.md 2/benchmarks/results/2026-09-10-swebench-restored-three-arm-five.md
new file mode 100644
index 0000000..8202ba5
--- /dev/null
+++ 2/benchmarks/results/2026-09-10-swebench-restored-three-arm-five.md
@@ -0,0 +1,83 @@
+# Restored SWE-bench five-instance three-arm comparison
+
+## Scope
+
+All arms ran the same five mini-suite instances inside their official
+SWE-bench images and were scored by SWE-bench 5.0.2:
+
+- `django__django-11815`
+- `django__django-12155`
+- `django__django-12708`
+- `sphinx-doc__sphinx-8035`
+- `sphinx-doc__sphinx-9367`
+
+Shared model family: Claude Opus 4.8. Daimonos and mini-swe-agent used the exact
+OpenRouter slug `anthropic/claude-opus-4.8`; Cursor used its corresponding
+`claude-opus-4-8-medium` backend model.
+
+Versions:
+
+- Daimonos commit `827d461c006d0d91af7e6a4b9c94ef2d54549534`
+- Daimonos musl binary
+  `bfd61ce4edea4e118c3a1a56fd7e250a31dd60bc89b9a4d5d2c8538bbc0983c2`
+- Cursor CLI `2026.09.02-c22c1a3`
+- mini-swe-agent `2.4.6`
+- CPython `3.12.14`
+- enriched dataset
+  `6ce05e6b926c91faecdbb4243014ac85dca471dfad30ccdb69453343b75267c3`
+
+Each arm has one repetition. This is a restored-harness validation, not an
+optimization-lineage stage or publishable aggregate claim.
+
+## Aggregate results
+
+| Harness | Tokens | LLM calls | Measured cost | Agent wall | Resolved |
+|---|---:|---:|---:|---:|---:|
+| Daimonos | 483,581 | 32 | $2.521245 | 94.638 s | 5/5 |
+| mini-swe-agent | 755,633 | 88 | $1.090288 | 267.544 s | 5/5 |
+| Cursor | 1,307,205 | unavailable | unavailable | 261.703 s | 5/5 |
+
+Daimonos used 36.00% fewer raw tokens and 64.63% less agent wall time than
+mini-swe-agent, and 63.01% fewer raw tokens and 63.84% less agent wall time than
+Cursor in this repetition.
+
+Those token ratios are not cost ratios. mini-swe-agent explicitly used
+`set_cache_control=default_end`, Cursor reported 1,164,639 cache-read tokens,
+and Daimonos reported no cache reads. mini-swe-agent therefore cost 56.76% less
+than Daimonos despite using more raw tokens. Cursor bills through Cursor's
+backend, so its USD cost is unavailable. Cache policy is a harness-default
+confound and must be controlled before making a cost-efficiency claim.
+
+## Per-instance results
+
+| Instance | Daimonos tokens | mini tokens | Cursor tokens | D cost | mini cost |
+|---|---:|---:|---:|---:|---:|
+| `django__django-11815` | 54,780 | 55,375 | 90,989 | $0.282880 | $0.122262 |
+| `django__django-12155` | 53,764 | 38,968 | 88,934 | $0.276360 | $0.082542 |
+| `django__django-12708` | 89,435 | 243,253 | 117,593 | $0.464875 | $0.347552 |
+| `sphinx-doc__sphinx-8035` | 205,440 | 399,300 | 921,135 | $1.083820 | $0.482990 |
+| `sphinx-doc__sphinx-9367` | 80,162 | 18,737 | 88,554 | $0.413310 | $0.054942 |
+
+All 15 patches resolved. No evaluator infrastructure failures, ambiguous
+failures, errors, or leaked containers occurred.
+
+## Run artifacts and caveats
+
+- Daimonos:
+  `results/20260910-130053-swebench-docker-compare-five-r1`, with
+  `sphinx-9367` replaced by the budget-settled retry in
+  `results/20260910-130512-swebench-docker-compare-five-r1-retry9367`.
+- Cursor:
+  `results/20260910-130704-swebench-cursor-docker-compare-five-r1`.
+- mini-swe-agent: `results/mini-compare-five-r1`.
+
+The first Daimonos `sphinx-9367` attempt was rejected before inference with
+OpenRouter HTTP 402 `in_flight_budget_exhausted`; it consumed zero tokens and
+zero cost. The replacement ran after the stated 120-second settlement window.
+
+Cursor's extractor currently reports no LLM-call/tool-call count, so those
+fields must not be treated as zero. mini-swe-agent cost comes from each
+trajectory's `model_stats.instance_cost`.
+
+At least two more matched repetitions, plus an explicit cache-policy decision,
+are needed before interpreting relative efficiency.

### Known Concerns
1. The Daimonos aggregate splices one zero-token budget-rejected sphinx-9367 attempt with its successful retry.
2. Cache policy differs materially across harness defaults, so token and cost rankings are not equivalent.
3. Cursor call/tool counts and USD cost are unavailable.
4. Each comparison arm has one full five-instance repetition.

### Specific Questions for Reviewer
1. Are all arithmetic and correctness statements supported by artifacts?
2. Are cache, retry, and sample-size confounds prominent enough to prevent overclaiming?
3. Is deferring the full 50 justified?

<!-- event id="review-1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S1. Per-instance Daimonos tokens sum to 483,581 and costs to $2.521245, matching the aggregate row, and the mini-swe-agent per-instance costs sum to $1.090288; however, the aggregate splice of the zero-token budget-rejected sphinx-9367 attempt with its retry should be labeled directly in the aggregate table (e.g., a footnote marker on the Daimonos row), not only in the caveats section, so a reader scanning the table cannot miss that the Daimonos aggregate is a composite of two run directories.
S2. State explicitly in the comparison doc which cache-policy control will be adopted for the planned matched repetitions (e.g., force set_cache_control off for mini-swe-agent, or enable equivalent caching in Daimonos), rather than only noting that a decision is needed; committing to the control now prevents the confound from silently persisting into the next round.
S3. In the README, note that Cursor call/tool counts are 'unavailable' rather than zero at the point where run_cursor.py is introduced, mirroring the comparison doc's warning, so downstream users of ../analyze.py do not misread absent fields.
S4. Deferring the full 50-instance run is justified given single-repetition arms and the uncontrolled cache confound; consider adding one sentence in the comparison doc stating the concrete precondition for scheduling the full 50 (cache decision made plus ≥3 matched five-instance repetitions per arm) so the deferral has an explicit exit criterion.

**Verdict:** APPROVE
