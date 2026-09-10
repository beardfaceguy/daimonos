# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1450-restored-five-reports" -->

<!-- event id="request" artifact path="1450-restored-five-reports/artifacts/round-1-review-request.diff" sha256="d04397856e21f2fc5391750d1f2486cd42347f7acd811db2291ad7e7e9c7cd4c" -->
## Review Request — Round 1
**Task:** 1450 — Record restored five-instance SWE-bench R1 and R2
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Commit two durable, per-run reports preserving exact scope fingerprints, binary/dataset/task-set identities, every per-task result, correctness outcomes, measured cost, historical comparison caveats, and the high-variance sphinx-8035 and failed django-11815 observations without adding an optimization-lineage claim.

### Relevant Code / Diff
diff --git 1/benchmarks/results/2026-09-10-swebench-restored-five-r1.md 2/benchmarks/results/2026-09-10-swebench-restored-five-r1.md
new file mode 100644
index 0000000..7779261
--- /dev/null
+++ 2/benchmarks/results/2026-09-10-swebench-restored-five-r1.md
@@ -0,0 +1,60 @@
+# SWE-bench restored five-instance run — R1
+
+## Scope
+
+- Runtime: Daimonos inside official SWE-bench Docker images
+- Provider/model: OpenRouter, `anthropic/claude-opus-4.8`
+- Compaction: off
+- Daimonos commit: `da9fc5e13582922e90714e0ba88591416b4f0488`
+- musl binary SHA-256:
+  `e024cb9c9de3083fa5d2e80d722f52d19f9e1d1ded816577c4b5b33e9a70956f`
+- Enriched mini-dataset SHA-256:
+  `6ce05e6b926c91faecdbb4243014ac85dca471dfad30ccdb69453343b75267c3`
+- Task-set SHA-256:
+  `b8949f60c948b5217a8803e7cb9d624f81aa35bb467aff9409a8f11eb3ed3832`
+- Repetitions: one
+- Raw run:
+  `benchmarks/swebench/results/20260910-115346-swebench-docker-restored-five-r1`
+- Evaluation report:
+  `benchmarks/swebench/daimonos-anthropic__claude-opus-4.8.20260910-115346-swebench-docker-restored-five-r1.json`
+
+This is a restoration/reproducibility run, not an optimization-lineage stage.
+It does not establish a savings or regression claim at one repetition.
+
+## Results
+
+| Instance | Tokens | LLM calls | Cost | Agent wall | Patch | Resolved |
+|---|---:|---:|---:|---:|---:|---:|
+| `django__django-11815` | 54,967 | 4 | $0.285315 | 10.553 s | 779 B | no |
+| `django__django-12155` | 53,737 | 4 | $0.275685 | 9.379 s | 666 B | yes |
+| `django__django-12708` | 88,818 | 6 | $0.460830 | 15.402 s | 787 B | yes |
+| `sphinx-doc__sphinx-8035` | 381,756 | 16 | $1.977600 | 58.109 s | 3,922 B | yes |
+| `sphinx-doc__sphinx-9367` | 66,797 | 5 | $0.344345 | 10.978 s | 785 B | yes |
+| **Total** | **646,075** | **35** | **$3.343775** | **104.421 s** |  | **4/5** |
+
+All five patches applied and evaluated. There were no infrastructure failures,
+ambiguous failures, empty patches, evaluator errors, or leaked containers.
+
+`django__django-11815` failed one of its two FAIL_TO_PASS tests:
+`test_serialize_enums`. Its patch used `enum_class.__qualname__`; the two earlier
+smoke runs used `enum_class.__name__` and resolved the instance.
+
+## Historical one-pass comparison
+
+The prior README five-instance run reported 436,834 tokens, approximately
+$1.53, 84 seconds, and 5/5 resolved. This run observed:
+
+- tokens: +47.90%;
+- measured cost: +118.55%;
+- summed agent wall time: +24.31%;
+- correctness: 4/5 instead of 5/5.
+
+Four task token totals were within ±0.4% of the prior run. The aggregate increase
+comes almost entirely from `sphinx-doc__sphinx-8035`, which rose from 172,729
+tokens / 10 calls to 381,756 tokens / 16 calls (+121.01%). Historical notes
+already record a 503k-token run for that same instance, so this is consistent
+with known strategy variance rather than enough evidence for a harness
+regression.
+
+At least one additional repetition is required before interpreting aggregate
+movement. Every per-task regression remains visible above.
diff --git 1/benchmarks/results/2026-09-10-swebench-restored-five-r2.md 2/benchmarks/results/2026-09-10-swebench-restored-five-r2.md
new file mode 100644
index 0000000..0084a19
--- /dev/null
+++ 2/benchmarks/results/2026-09-10-swebench-restored-five-r2.md
@@ -0,0 +1,66 @@
+# SWE-bench restored five-instance run — R2
+
+## Scope
+
+Configuration matches R1:
+
+- Daimonos commit: `da9fc5e13582922e90714e0ba88591416b4f0488`
+- musl binary SHA-256:
+  `e024cb9c9de3083fa5d2e80d722f52d19f9e1d1ded816577c4b5b33e9a70956f`
+- OpenRouter model: `anthropic/claude-opus-4.8`
+- Compaction: off
+- Official SWE-bench Docker images
+- Enriched mini-dataset SHA-256:
+  `6ce05e6b926c91faecdbb4243014ac85dca471dfad30ccdb69453343b75267c3`
+- Task-set SHA-256:
+  `b8949f60c948b5217a8803e7cb9d624f81aa35bb467aff9409a8f11eb3ed3832`
+- Raw run:
+  `benchmarks/swebench/results/20260910-120053-swebench-docker-restored-five-r2`
+
+This is a restoration/reproducibility run, not an optimization-lineage stage.
+
+## Results
+
+| Instance | Tokens | LLM calls | Cost | Agent wall | Patch | Resolved |
+|---|---:|---:|---:|---:|---:|---:|
+| `django__django-11815` | 26,702 | 3 | $0.136650 | 6.602 s | empty | no |
+| `django__django-12155` | 53,764 | 4 | $0.276360 | 8.376 s | 666 B | yes |
+| `django__django-12708` | 89,167 | 6 | $0.461155 | 17.477 s | 839 B | yes |
+| `sphinx-doc__sphinx-8035` | 521,539 | 25 | $2.695695 | 79.222 s | 4,446 B | yes |
+| `sphinx-doc__sphinx-9367` | 56,811 | 4 | $0.293915 | 9.531 s | 785 B | yes |
+| **Total** | **747,983** | **42** | **$3.863775** | **121.208 s** |  | **4/5** |
+
+The `django__django-11815` generation ended before producing a patch. OpenRouter
+returned HTTP 400 after two tool-loop calls because the provider rejected an
+assistant-prefill conversation: “The conversation must end with a user
+message.” The task summary correctly records `exit_code=1`, `is_error=true`,
+`failed_calls=1`, and an empty patch. The evaluator excluded that patch and
+resolved all four submitted non-empty patches.
+
+There were no evaluator infrastructure failures, ambiguous failures, evaluator
+errors, or leaked containers.
+
+## R2 versus R1
+
+- total tokens: +15.77%;
+- measured cost: +15.55%;
+- summed agent wall: +16.08%;
+- strict correctness: 4/5 in both runs, with different failure modes for
+  `django__django-11815`.
+
+The increase again comes from `sphinx-doc__sphinx-8035`: 521,539 tokens and 25
+calls versus R1's 381,756 and 16 (+36.62% tokens). Across R1 and R2 that single
+instance ranges from 382k to 522k tokens, consistent with the previously
+recorded 173k–503k spread.
+
+The other successful instances remained comparatively stable:
+
+- `django__django-12155`: +0.05% tokens;
+- `django__django-12708`: +0.39%;
+- `sphinx-doc__sphinx-9367`: -14.95%.
+
+Across the two restored repetitions, strict correctness is 8/10, mean total
+tokens are 697,029, mean cost is $3.603775, and mean summed agent wall is
+112.815 seconds. This sample establishes that the restored harness executes and
+accounts correctly, but it is too small and too strategy-sensitive for an
+aggregate efficiency claim.

### Known Concerns
1. R1 and R2 are single repetitions and cannot support aggregate efficiency claims.
2. The historical comparison uses a prior README summary rather than recovered raw runs.
3. The R2 evaluator reports four completed/resolved patches plus one empty patch, so strict suite correctness is recorded as 4/5.

### Specific Questions for Reviewer
1. Are arithmetic, correctness interpretation, and caveats accurate?
2. Do the reports preserve every regression without overstating comparability?
3. Should these restoration reports remain outside optimization-lineage.json?

<!-- event id="review-1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S1. Arithmetic checks out: R1 totals (646,075 tokens; 35 calls; $3.343775; 104.421 s) and R2 totals (747,983 tokens; 42 calls; $3.863775; 121.208 s) match their per-task rows, and the derived deltas (+47.90% tokens, +118.55% cost, +24.31% wall vs. the historical 436,834/$1.53/84 s baseline; +15.77%/+15.55%/+16.08% R2 vs R1) are correct, as are the two-run means (697,029 tokens; $3.603775; 112.815 s). No changes needed, but consider noting explicitly in R1 that the historical baseline cost is approximate, so the +118.55% figure inherits that imprecision.
S2. Correctness interpretation is sound: recording R2 as strict 4/5 despite the evaluator resolving all four submitted patches is the right conservative treatment of the empty-patch failure, and distinguishing the R1 test failure (__qualname__ vs __name__) from the R2 provider HTTP 400 failure mode is valuable. Optionally, add a one-line pointer to whether the assistant-prefill HTTP 400 is tracked as a separate harness/provider issue, since it is an infrastructure-adjacent failure rather than a model-strategy failure.
S3. Keeping these restoration reports outside optimization-lineage.json is correct: single repetitions with known high variance on sphinx-8035 (173k–522k token spread) cannot support efficiency or regression claims, and both reports state this explicitly. The caveats about the README-summary historical baseline (not recovered raw runs) are appropriately preserved. If a future R3+ aggregate is produced, consider cross-linking these reports from it rather than retrofitting them into the lineage.

**Verdict:** APPROVE
