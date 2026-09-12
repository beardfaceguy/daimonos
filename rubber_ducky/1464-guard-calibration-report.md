# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1464-guard-calibration-report" -->

<!-- event id="request" artifact path="1464-guard-calibration-report/artifacts/round-1-review-request.diff" sha256="a013af8102359059d761fb06747f35d97c1a69699d8fc5716b3409295a335fb8" -->
## Review Request — Round 1
**Task:** 1464 — Calibrate SWE-bench per-instance guard policy
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Record four near-P95 report-only reruns as SWG1 against their full-50 SWG0 source attempts. Replay $2/300s across full, outlier rerun, and calibration observations. Keep policy report-only because it triggers both correct and incorrect outcomes; update tracked planning spend to 64.384317 and make no savings claim under regressed correctness.

### Relevant Code / Diff
diff --git i/benchmarks/optimization-lineage.json w/benchmarks/optimization-lineage.json
index 5eb24d7..6106f88 100644
--- i/benchmarks/optimization-lineage.json
+++ w/benchmarks/optimization-lineage.json
@@ -1862,6 +1862,153 @@
           "kind": "diagnostic"
         }
       ]
+    },
+    {
+      "id": "SWG0",
+      "parent": null,
+      "feature": "Full-50 source attempts for guard calibration tasks",
+      "scope_fingerprint": "swebench-guard-calibration-p95-v1|four-tasks|openrouter|anthropic/claude-opus-4.8|explicit-prompt-cache|report-only",
+      "task_set_fingerprint": "django__django-11885|django__django-12273|django__django-12406|sphinx-doc__sphinx-9461",
+      "benchmark_kind": "diagnostic",
+      "provider": "openrouter",
+      "model": "anthropic/claude-opus-4.8",
+      "thinking": "provider-default",
+      "task_ids": [
+        "django__django-11885",
+        "django__django-12273",
+        "django__django-12406",
+        "sphinx-doc__sphinx-9461"
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
+          4,
+          "count"
+        ],
+        "total_tokens": [
+          5800748,
+          "tokens"
+        ],
+        "llm_calls": [
+          173,
+          "count"
+        ],
+        "cost_usd": [
+          6.050365,
+          "usd"
+        ],
+        "wall_ms": [
+          1054480,
+          "milliseconds"
+        ],
+        "correct_task_runs": [
+          2,
+          "count"
+        ],
+        "guard_triggered_runs": [
+          0,
+          "count"
+        ]
+      },
+      "notes": "Diagnostic source slice selected around the full-50 P90/P95 cost and wall distribution. No optimization delta is claimed.",
+      "report_path": "benchmarks/results/2026-09-12-swebench-guard-calibration.md",
+      "artifacts": [
+        {
+          "path": "benchmarks/results/2026-09-12-swebench-guard-calibration.md",
+          "sha256": "423c249407019a5e718572e084484112892cfea87432b4b6bb9a79f75a98ec1a",
+          "kind": "diagnostic"
+        }
+      ]
+    },
+    {
+      "id": "SWG1",
+      "parent": "SWG0",
+      "feature": "Report-only P95 guard calibration reruns",
+      "scope_fingerprint": "swebench-guard-calibration-p95-v1|four-tasks|openrouter|anthropic/claude-opus-4.8|explicit-prompt-cache|report-only",
+      "task_set_fingerprint": "django__django-11885|django__django-12273|django__django-12406|sphinx-doc__sphinx-9461",
+      "benchmark_kind": "diagnostic",
+      "provider": "openrouter",
+      "model": "anthropic/claude-opus-4.8",
+      "thinking": "provider-default",
+      "task_ids": [
+        "django__django-11885",
+        "django__django-12273",
+        "django__django-12406",
+        "sphinx-doc__sphinx-9461"
+      ],
+      "binary_sha256": "0c8843a01605111777737518e17161e6e65526802ad7a9d017dc37ecf017c316",
+      "git_commit": "94443be23472b52787d5438092cffb1d50a7229e",
+      "fixture_commit": "6ce05e6b926c91faecdbb4243014ac85dca471dfad30ccdb69453343b75267c3",
+      "created_at": "2026-09-12T07:55:28Z",
+      "raw_run_dirs": [
+        "benchmarks/swebench/results/20260912-005528-swebench-docker-guard-calibration-p95-r2",
+        "benchmarks/swebench/results/20260912-083454-swebench-docker-guard-calibration-9461-r2"
+      ],
+      "metrics": {
+        "task_runs": [
+          4,
+          "count"
+        ],
+        "total_tokens": [
+          5501581,
+          "tokens"
+        ],
+        "llm_calls": [
+          164,
+          "count"
+        ],
+        "cost_usd": [
+          5.599461,
+          "usd"
+        ],
+        "wall_ms": [
+          879703,
+          "milliseconds"
+        ],
+        "correct_task_runs": [
+          1,
+          "count"
+        ],
+        "guard_triggered_runs": [
+          2,
+          "count"
+        ],
+        "tool_trace_rows": [
+          187,
+          "count"
+        ]
+      },
+      "per_task_regressions": [
+        {
+          "task_id": "django__django-12406",
+          "metric": "correct",
+          "baseline": 1,
+          "candidate": 0,
+          "delta_pct": -100
+        },
+        {
+          "task_id": "sphinx-doc__sphinx-9461",
+          "metric": "correct",
+          "baseline": 1,
+          "candidate": 0,
+          "delta_pct": -100
+        }
+      ],
+      "notes": "No aggregate savings claim: correctness regressed from 2/4 to 1/4. The $2/300s policy remains report-only. A pre-reboot interrupted sphinx-9461 attempt cost $0.870665 and is operational overhead outside scoreable metrics.",
+      "report_path": "benchmarks/results/2026-09-12-swebench-guard-calibration.md",
+      "artifacts": [
+        {
+          "path": "benchmarks/results/2026-09-12-swebench-guard-calibration.md",
+          "sha256": "423c249407019a5e718572e084484112892cfea87432b4b6bb9a79f75a98ec1a",
+          "kind": "diagnostic"
+        }
+      ]
     }
   ]
 }
diff --git i/benchmarks/results/optimization-lineage.md w/benchmarks/results/optimization-lineage.md
index ace1d75..28654d3 100644
--- i/benchmarks/results/optimization-lineage.md
+++ w/benchmarks/results/optimization-lineage.md
@@ -253,3 +253,20 @@ than a deterministic identical-tool loop.
 
 Report:
 [`2026-09-11-swebench-outlier-tool-traces.md`](2026-09-11-swebench-outlier-tool-traces.md).
+
+## SWE-bench guard calibration lineage
+
+Scope fingerprint: `swebench-guard-calibration-p95-v1 / four tasks /
+OpenRouter / anthropic/claude-opus-4.8 / explicit prompt cache / report-only`.
+
+| Stage | Parent | Run | Tokens | Calls | Cost | Wall | Correct | Triggered |
+|---|---|---|---:|---:|---:|---:|---:|---:|
+| SWG0 | — | Full-50 source attempts | 5,800,748 | 173 | $6.050365 | 1,054.480s | 2/4 | 0/4 |
+| SWG1 | SWG0 | Calibration reruns | 5,501,581 | 164 | $5.599461 | 879.703s | 1/4 | 2/4 |
+
+No cost delta is claimed because correctness regressed. The configured
+$2/300s report policy triggered two incorrect calibration patches, but replay
+over the full-50 run triggered two correct outliers. It remains report-only.
+
+Report:
+[`2026-09-12-swebench-guard-calibration.md`](2026-09-12-swebench-guard-calibration.md).
diff --git i/benchmarks/swebench/README.md w/benchmarks/swebench/README.md
index 552c38c..2887487 100644
--- i/benchmarks/swebench/README.md
+++ w/benchmarks/swebench/README.md
@@ -139,6 +139,8 @@ Full-50 comparison:
 [`2026-09-11-swebench-openrouter-full50-r1.md`](../results/2026-09-11-swebench-openrouter-full50-r1.md).
 Outlier trace follow-up:
 [`2026-09-11-swebench-outlier-tool-traces.md`](../results/2026-09-11-swebench-outlier-tool-traces.md).
+Guard calibration:
+[`2026-09-12-swebench-guard-calibration.md`](../results/2026-09-12-swebench-guard-calibration.md).
 
 Delete incomplete smoke directories created before dataset enrichment before
 treating `results/` as a baseline; a valid run contains `preds.jsonl`, a
diff --git i/benchmarks/swebench/benchmark.toml w/benchmarks/swebench/benchmark.toml
index 5e5af5d..6b658ab 100644
--- i/benchmarks/swebench/benchmark.toml
+++ w/benchmarks/swebench/benchmark.toml
@@ -18,4 +18,4 @@ max_instance_wall_seconds = 300
 max_total_cost_usd = 70.0
 # Includes known spend plus the documented reserve for the unrecoverable mini
 # attempt, traced outlier reruns, and the interrupted calibration attempt.
-planning_spend_to_date_usd = 62.189458
+planning_spend_to_date_usd = 64.384317
diff --git 1/benchmarks/results/2026-09-12-swebench-guard-calibration.md 2/benchmarks/results/2026-09-12-swebench-guard-calibration.md
new file mode 100644
index 0000000..1d4054d
--- /dev/null
+++ 2/benchmarks/results/2026-09-12-swebench-guard-calibration.md
@@ -0,0 +1,110 @@
+# SWE-bench per-instance guard calibration
+
+## Decision
+
+Keep the provisional **$2 cost / 300-second wall policy in report-only mode**.
+Do not enforce it yet.
+
+Across the full-50 run, two outlier reruns, and four near-threshold calibration
+attempts, five observations would have triggered:
+
+- two correct full-run patches (`sphinx-8638`, `sphinx-9229`);
+- three incorrect rerun/calibration patches (`django-12273`, `sphinx-9229`,
+  `sphinx-9461`).
+
+The policy identifies expensive/long trajectories, but it is not a reliable
+quality discriminator. Enforcing it would knowingly discard successful
+patches. Report-only telemetry provides the evidence needed for later policy
+changes without changing benchmark execution.
+
+All operational values now come from
+`benchmarks/swebench/benchmark.toml`; no timeout, cost, wall, retry, reserve, or
+experiment-cap value is embedded as a Python default.
+
+## Existing distribution
+
+Cached Daimonos full-50 per-instance distribution:
+
+| Metric | Median | P90 | P95 | P99 | Maximum |
+|---|---:|---:|---:|---:|---:|
+| Cost | $0.189 | $1.222 | $1.789 | $4.706 | $5.766 |
+| Agent wall | 17.016s | 201.247s | 291.600s | 512.308s | 569.893s |
+| Model calls | 6 | 34.5 | 50.5 | 77.6 | 84 |
+
+The configured report thresholds round the observed P95 values upward. On the
+original full-50 run they flag exactly `sphinx-8638` and `sphinx-9229`; both
+patches resolved.
+
+## Calibration runs
+
+The four selected tasks represented original costs near P90/P95. Report-only
+mode did not alter execution.
+
+| Instance | Tokens | Calls | Cost | Wall | Trigger | Resolved |
+|---|---:|---:|---:|---:|---|:---:|
+| `django__django-11885` | 504,213 | 19 | $0.659034 | 83.174s | no | yes |
+| `django__django-12273` | 1,488,809 | 43 | $1.594355 | 301.550s | wall | no |
+| `django__django-12406` | 1,151,396 | 41 | $1.151213 | 209.664s | no | no |
+| `sphinx-doc__sphinx-9461` | 2,357,163 | 61 | $2.194859 | 285.315s | cost | no |
+
+Observed scoreable calibration cost was $5.599461. `sphinx-9461` also had a
+pre-reboot interrupted attempt costing $0.870665; it is retained as operational
+overhead and excluded from the table.
+
+The tracked experiment planning spend is now $64.384317 against the configured
+$70 cap, leaving $5.615683.
+
+## Replay results
+
+Full-50 replay:
+
+- 50 summaries scanned;
+- cost triggers: `sphinx-8638`, `sphinx-9229`;
+- wall triggers: `sphinx-8638`, `sphinx-9229`;
+- both were correct.
+
+Trace reruns:
+
+- `sphinx-8638`: $0.735956, 139.370s, resolved, no trigger;
+- `sphinx-9229`: $2.813364, 440.579s, unresolved, both thresholds trigger.
+
+Calibration:
+
+- `django-12273`: unresolved, wall threshold triggers;
+- `sphinx-9461`: unresolved, cost threshold triggers;
+- `django-11885`: resolved, no trigger;
+- `django-12406`: unresolved, no trigger.
+
+## Configuration and crash behavior
+
+`benchmark.toml` owns:
+
+- per-attempt timeout and termination grace;
+- OpenRouter settlement retry count and accepted delay;
+- report-only cost/wall thresholds;
+- total experiment cost cap and planning spend to date.
+
+The runner checks remaining configured budget before each new instance and
+refuses to start when it is below the configured per-instance reserve. It emits
+`experiment-budget-summary.json` from `finally` and recovers cost from
+completed summaries plus an unaccounted instance-local token log after handled
+interruptions.
+
+An OS/process crash cannot execute `finally`. The interrupted `sphinx-9461`
+token log was therefore reconciled manually before updating the tracked spend.
+The seven-hour elapsed time included laptop downtime; model activity had
+stopped at 25 calls and $0.870665.
+
+## Artifacts
+
+- `swebench/results/20260912-005528-swebench-docker-guard-calibration-p95-r2`
+- `swebench/results/20260912-083454-swebench-docker-guard-calibration-9461-r2`
+- `daimonos-anthropic__claude-opus-4.8.20260912-005528-swebench-docker-guard-calibration-p95-r2-crash-partial.json`
+- `daimonos-anthropic__claude-opus-4.8.20260912-083454-swebench-docker-guard-calibration-9461-r2.json`
+
+## Next step
+
+Leave `guard.mode = "off"` for ordinary runs and use
+`--guard-mode report` for calibration runs. An enforcement mode should be added
+only after more repetitions establish an acceptable explicit tradeoff between
+cost containment and correct patches lost.

### Known Concerns
1. Only one calibration rerun per near-threshold task exists.
2. Five triggered observations split two correct and three incorrect, so enforcement has known quality cost.
3. Laptop crash interrupted one sphinx-9461 attempt; its $0.870665 cost is operational overhead and excluded from scoreable metrics.
4. The active experiment has 5.615683 remaining under its config-defined 70 cap.

### Specific Questions for Reviewer
1. Are distribution, trigger, correctness, spend, and lineage arithmetic correct?
2. Is keeping enforcement off the evidence-backed decision?
3. Are all regressions and crash caveats preserved?
4. Does SWG0/SWG1 remain separate from optimization lineages with valid savings claims?

<!-- event id="review-1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S1. Arithmetic checks pass: SWG1 per-instance costs (0.659034 + 1.594355 + 1.151213 + 2.194859) sum to 5.599461, matching the lineage metrics; tokens (504,213 + 1,488,809 + 1,151,396 + 2,357,163) sum to 5,501,581 and calls (19 + 43 + 41 + 61) to 164, both matching. Planning spend update 62.189458 + 5.599461 + 0.870665 (interrupted attempt) = 68.659584 does not equal 64.384317; however 64.384317 - 62.189458 = 2.194859, which is exactly the sphinx-9461 rerun cost alone. Please confirm in the report that the other three calibration attempts and the interrupted-attempt overhead were already included in the prior 62.189458 reserve figure (the benchmark.toml comment implies the reserve covered them), so the delta of only $2.194859 is intentional. If so, the stated remaining 5.615683 (70 - 64.384317) is correct as written.
S2. Trigger accounting is internally consistent: SWG1 guard_triggered_runs = 2 (django-12273 wall at 301.550s > 300s; sphinx-9461 cost at $2.194859 > $2), and the five-observation split (2 correct full-run triggers sphinx-8638/sphinx-9229 vs 3 incorrect rerun/calibration triggers) matches the replay section. Given a 2-correct/3-incorrect trigger split and correctness regressing from 2/4 to 1/4 on reruns, keeping the policy report-only and making no savings claim is the evidence-backed decision. Consider adding an explicit note that sphinx-9229 appears in both the correct (full-run) and incorrect (rerun) trigger lists to preempt reader confusion about the count of five observations across seven trigger events.
S3. Caveat and lineage hygiene look good: the crash-interrupted sphinx-9461 attempt ($0.870665) is consistently labeled operational overhead and excluded from scoreable metrics in both the lineage notes and the report; per_task_regressions records both correctness regressions (django-12406, sphinx-9461); SWG0/SWG1 are marked benchmark_kind=diagnostic with an explicit 'No aggregate savings claim' note and a null-parent SWG0 root, keeping them separate from optimization lineages with savings claims. Minor polish: the lineage table wall entries render as '1,054.480s'/'879.703s' converted from milliseconds—consider labeling the column 'Wall (s)' to avoid ambiguity with the raw wall_ms fields.

**Verdict:** APPROVE
