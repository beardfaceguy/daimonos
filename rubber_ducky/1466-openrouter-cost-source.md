# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1466-openrouter-cost-source" -->

<!-- event id="request" artifact path="1466-openrouter-cost-source/artifacts/round-1-review-request.diff" sha256="9d42463b0950e0c29ab447d6b145c52fedfd91640432bbe8dcd4165ea135b648" -->
## Review Request — Round 1
**Task:** 1466 — Use OpenRouter as the common comparison cost source
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Normalize mini-swe-agent trajectories by summing the provider-returned OpenRouter usage.cost on every generation, report cost only when every usage-bearing call contains a valid cost, retain LiteLLM model_stats only as a consistency check, and update the restored comparison interpretation to rank default-harness costs while treating cache policy as causal attribution rather than an accounting mismatch.

### Relevant Code / Diff
diff --git i/benchmarks/results/2026-09-10-swebench-restored-three-arm-five.md w/benchmarks/results/2026-09-10-swebench-restored-three-arm-five.md
index 8202ba5..b389e56 100644
--- i/benchmarks/results/2026-09-10-swebench-restored-three-arm-five.md
+++ w/benchmarks/results/2026-09-10-swebench-restored-three-arm-five.md
@@ -41,12 +41,18 @@ Daimonos used 36.00% fewer raw tokens and 64.63% less agent wall time than
 mini-swe-agent, and 63.01% fewer raw tokens and 63.84% less agent wall time than
 Cursor in this repetition.
 
-Those token ratios are not cost ratios. mini-swe-agent explicitly used
-`set_cache_control=default_end`, Cursor reported 1,164,639 cache-read tokens,
-and Daimonos reported no cache reads. mini-swe-agent therefore cost 56.76% less
-than Daimonos despite using more raw tokens. Cursor bills through Cursor's
-backend, so its USD cost is unavailable. Cache policy is a harness-default
-confound and must be controlled before making a cost-efficiency claim.
+Those token ratios are not cost ratios. Daimonos and mini-swe-agent costs both
+sum OpenRouter's provider-returned per-generation `usage.cost`; mini's summed
+value also equals its independent `model_stats.instance_cost`. Under the
+tested harness defaults, mini-swe-agent therefore cost 56.76% less than
+Daimonos despite using more raw tokens.
+
+Caching explains the divergence rather than invalidating that default-harness
+cost comparison: mini-swe-agent used `set_cache_control=default_end`, Cursor
+reported 1,164,639 cache-read tokens, and Daimonos reported no cache reads.
+A cache-controlled experiment is required to attribute the difference, but not
+to answer which OpenRouter arm cost more as configured. Cursor bills through
+Cursor's backend, so its USD cost remains unavailable.
 
 ## Per-instance results
 
@@ -76,8 +82,10 @@ OpenRouter HTTP 402 `in_flight_budget_exhausted`; it consumed zero tokens and
 zero cost. The replacement ran after the stated 120-second settlement window.
 
 Cursor's extractor currently reports no LLM-call/tool-call count, so those
-fields must not be treated as zero. mini-swe-agent cost comes from each
-trajectory's `model_stats.instance_cost`.
+fields must not be treated as zero. mini-swe-agent cost comes from summing the
+same OpenRouter `usage.cost` field Daimonos records; LiteLLM's aggregate is
+retained only as a consistency check.
 
-At least two more matched repetitions, plus an explicit cache-policy decision,
-are needed before interpreting relative efficiency.
+At least two more matched repetitions are needed before publishing relative
+efficiency. An explicit cache-policy decision is needed only for causal
+attribution, not for the harness-default cost ranking above.
diff --git i/benchmarks/swebench/README.md w/benchmarks/swebench/README.md
index 54ad763..237ecfe 100644
--- i/benchmarks/swebench/README.md
+++ w/benchmarks/swebench/README.md
@@ -66,9 +66,10 @@ Each run writes `results/<run-id>/` with per-instance token/cost JSONs
 (same schema as the in-house suite — `../analyze.py results/` works),
 `.patch` files, raw transcripts, and `preds.jsonl`.
 mini-swe-agent writes trajectories plus `preds.json`; normalize each trajectory
-through `extract_mini.py` before cross-harness token analysis. Cursor uses its
-own backend, so token/correctness comparisons are available but USD cost parity
-is not.
+through `extract_mini.py` before cross-harness analysis. The normalizer sums
+OpenRouter's per-generation `usage.cost`, the same accounting source Daimonos
+uses; LiteLLM's aggregate is not used for ranking. Cursor uses its own backend,
+so token/correctness comparisons are available but USD cost parity is not.
 
 Delete incomplete smoke directories created before dataset enrichment before
 treating `results/` as a baseline; a valid run contains `preds.jsonl`, a
diff --git i/benchmarks/swebench/extract_mini.py w/benchmarks/swebench/extract_mini.py
index f880e57..b8d5ade 100644
--- i/benchmarks/swebench/extract_mini.py
+++ w/benchmarks/swebench/extract_mini.py
@@ -13,6 +13,8 @@ def main():
     traj_path, iid, repo, model, out_path = sys.argv[1:6]
     t = json.load(open(traj_path))
     tot_in = tot_out = calls = 0
+    total_cost = 0.0
+    cost_complete = True
     timestamps = []
     for m in t.get("messages", []):
         extra = m.get("extra") or {}
@@ -21,6 +23,11 @@ def main():
             calls += 1
             tot_in += u.get("prompt_tokens", 0) or 0
             tot_out += u.get("completion_tokens", 0) or 0
+            cost = u.get("cost")
+            if isinstance(cost, (int, float)) and cost >= 0:
+                total_cost += cost
+            else:
+                cost_complete = False
         if "timestamp" in extra:
             timestamps.append(extra["timestamp"])
     wall_ms = int((max(timestamps) - min(timestamps)) * 1000) if len(timestamps) > 1 else 0
@@ -37,7 +44,10 @@ def main():
         "total_tokens": tot_in + tot_out,
         "prompt_tokens": tot_in,
         "llm_calls": calls,
-        "cost_usd": None,
+        # Same accounting source as Daimonos: OpenRouter's per-generation
+        # usage.cost, not mini-swe-agent/LiteLLM's model-price estimate.
+        "cost_usd": total_cost if calls and cost_complete else None,
+        "cost_source": "openrouter_usage" if calls and cost_complete else None,
         "exit_code": 0 if exit_status == "Submitted" else 1,
         "is_error": exit_status != "Submitted",
         "success": exit_status == "Submitted",
diff --git 1/tests/test_swebench_extract_mini.py 2/tests/test_swebench_extract_mini.py
new file mode 100644
index 0000000..0d5f3cf
--- /dev/null
+++ 2/tests/test_swebench_extract_mini.py
@@ -0,0 +1,134 @@
+"""Tests for mini-swe-agent SWE-bench summary normalization."""
+
+from __future__ import annotations
+
+import json
+import subprocess
+import sys
+from pathlib import Path
+
+import pytest
+
+
+SCRIPT = (
+    Path(__file__).resolve().parents[1] / "benchmarks" / "swebench" / "extract_mini.py"
+)
+
+
+def test_extract_mini_sums_openrouter_reported_cost(tmp_path):
+    trajectory = tmp_path / "trajectory.json"
+    output = tmp_path / "summary.json"
+    trajectory.write_text(
+        json.dumps(
+            {
+                "messages": [
+                    {
+                        "extra": {
+                            "timestamp": 10.0,
+                            "response": {
+                                "usage": {
+                                    "prompt_tokens": 100,
+                                    "completion_tokens": 10,
+                                    "cost": 0.0125,
+                                }
+                            },
+                        }
+                    },
+                    {
+                        "extra": {
+                            "timestamp": 12.5,
+                            "response": {
+                                "usage": {
+                                    "prompt_tokens": 120,
+                                    "completion_tokens": 20,
+                                    "cost": 0.0175,
+                                }
+                            },
+                        }
+                    },
+                ],
+                "info": {
+                    "exit_status": "Submitted",
+                    "model_stats": {"instance_cost": 0.03},
+                },
+            }
+        )
+    )
+
+    subprocess.run(
+        [
+            sys.executable,
+            str(SCRIPT),
+            str(trajectory),
+            "django__django-11815",
+            "django/django",
+            "openrouter/anthropic/claude-opus-4.8",
+            str(output),
+        ],
+        check=True,
+        capture_output=True,
+        text=True,
+        timeout=30,
+    )
+
+    summary = json.loads(output.read_text())
+    assert summary["total_tokens"] == 250
+    assert summary["llm_calls"] == 2
+    assert summary["cost_usd"] == pytest.approx(0.03)
+    assert summary["cost_source"] == "openrouter_usage"
+    assert summary["wall_ms"] == 2500
+
+
+def test_extract_mini_rejects_partial_provider_cost(tmp_path):
+    trajectory = tmp_path / "trajectory.json"
+    output = tmp_path / "summary.json"
+    trajectory.write_text(
+        json.dumps(
+            {
+                "messages": [
+                    {
+                        "extra": {
+                            "response": {
+                                "usage": {
+                                    "prompt_tokens": 100,
+                                    "completion_tokens": 10,
+                                    "cost": 0.0125,
+                                }
+                            }
+                        }
+                    },
+                    {
+                        "extra": {
+                            "response": {
+                                "usage": {
+                                    "prompt_tokens": 120,
+                                    "completion_tokens": 20,
+                                }
+                            }
+                        }
+                    },
+                ],
+                "info": {"exit_status": "Submitted"},
+            }
+        )
+    )
+
+    subprocess.run(
+        [
+            sys.executable,
+            str(SCRIPT),
+            str(trajectory),
+            "django__django-11815",
+            "django/django",
+            "openrouter/anthropic/claude-opus-4.8",
+            str(output),
+        ],
+        check=True,
+        capture_output=True,
+        text=True,
+        timeout=30,
+    )
+
+    summary = json.loads(output.read_text())
+    assert summary["cost_usd"] is None
+    assert summary["cost_source"] is None

### Known Concerns
1. Floating-point summation may differ by insignificant representation error.
2. Cursor remains outside USD comparison because it uses Cursor billing.
3. The comparison has one matched repetition and cannot support a stable publishable ranking.

### Specific Questions for Reviewer
1. Does mini extraction use the same accounting source as Daimonos without partial undercount?
2. Is the revised default-harness cost conclusion justified while preserving cache caveats?
3. Are tests sufficient for complete and partial usage.cost trajectories?

<!-- event id="review-1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. extract_mini.py treats a valid cost of exactly 0 the same as a positive cost, but more importantly it silently accepts booleans: isinstance(True, int) is True in Python, so usage.cost=true would be summed as 1.0. Guard with `isinstance(cost, (int, float)) and not isinstance(cost, bool)` before summing.
B2. The partial-cost gate only inspects usage-bearing calls. If a message has an `extra.response` with no `usage` object at all, it is skipped entirely and neither counted as a call nor marked cost-incomplete, so a trajectory with generations that lack usage blocks can still report a complete OpenRouter cost, undercounting exactly the way the proposal claims to prevent. Either count such responses and set cost_complete=False, or document why usage-less responses cannot carry billable generations.
B3. The results doc claims mini's summed usage.cost 'also equals its independent model_stats.instance_cost', and the README says LiteLLM's aggregate is 'retained only as a consistency check', but the extractor never performs or records that check. The consistency claim is unverifiable from the artifacts: either emit the model_stats value (e.g., a `cost_check` field or a warning on mismatch beyond tolerance) or drop the equality claim from the report.

### Non-Blocking Suggestions
S1. In the results doc, the strengthened claim that a cache-controlled experiment is not needed 'to answer which OpenRouter arm cost more as configured' is fine for this single repetition, but consider explicitly restating that the ranking is n=1 and provisional in the same paragraph, not only in the closing section, since the paragraph is the likely quotation target.
S2. Add a test for the negative-cost path (cost < 0 sets cost_usd/cost_source to None) and for a zero-call trajectory, since both branches of `calls and cost_complete` are otherwise only partially exercised.
S3. Consider math.fsum or accumulating into a list and summing once to address the acknowledged floating-point concern; it is cheap and removes the caveat from known_concerns.

**Verdict:** REVISE

<!-- event id="rebuttal-1" artifact path="1466-openrouter-cost-source/artifacts/round-1-rebuttal.diff" sha256="794c09411efe423e423b6d8e45728f285cb76799eca5fc78e89e57aced8b6f65" -->
## Rebuttal — Round 1
**Position:** AGREE

### Responses to Blocking Concerns
Re B1: ACCEPT — Excluded bool explicitly and retained nonnegative numeric validation; added boolean and negative-cost tests.
Re B2: ACCEPT — Every response-bearing message now counts as a generation; a missing, non-object, or empty usage block marks the provider cost incomplete. Added a missing-usage regression test.
Re B3: ACCEPT — Emitted model_stats_cost_usd and cost_matches_model_stats, with numeric validation and tolerance; added matching and mismatching tests and removed the independent wording.

### Revised Code / Diff
diff --git i/benchmarks/results/2026-09-10-swebench-restored-three-arm-five.md w/benchmarks/results/2026-09-10-swebench-restored-three-arm-five.md
index 8202ba5..1ccc212 100644
--- i/benchmarks/results/2026-09-10-swebench-restored-three-arm-five.md
+++ w/benchmarks/results/2026-09-10-swebench-restored-three-arm-five.md
@@ -41,12 +41,19 @@ Daimonos used 36.00% fewer raw tokens and 64.63% less agent wall time than
 mini-swe-agent, and 63.01% fewer raw tokens and 63.84% less agent wall time than
 Cursor in this repetition.
 
-Those token ratios are not cost ratios. mini-swe-agent explicitly used
-`set_cache_control=default_end`, Cursor reported 1,164,639 cache-read tokens,
-and Daimonos reported no cache reads. mini-swe-agent therefore cost 56.76% less
-than Daimonos despite using more raw tokens. Cursor bills through Cursor's
-backend, so its USD cost is unavailable. Cache policy is a harness-default
-confound and must be controlled before making a cost-efficiency claim.
+Those token ratios are not cost ratios. Daimonos and mini-swe-agent costs both
+sum OpenRouter's provider-returned per-generation `usage.cost`; mini's summed
+value also equals its trajectory-level `model_stats.instance_cost`. In this
+single, provisional matched repetition under the tested harness defaults,
+mini-swe-agent therefore cost 56.76% less than Daimonos despite using more raw
+tokens.
+
+Caching explains the divergence rather than invalidating that default-harness
+cost comparison: mini-swe-agent used `set_cache_control=default_end`, Cursor
+reported 1,164,639 cache-read tokens, and Daimonos reported no cache reads.
+A cache-controlled experiment is required to attribute the difference, but not
+to answer which OpenRouter arm cost more as configured. Cursor bills through
+Cursor's backend, so its USD cost remains unavailable.
 
 ## Per-instance results
 
@@ -76,8 +83,10 @@ OpenRouter HTTP 402 `in_flight_budget_exhausted`; it consumed zero tokens and
 zero cost. The replacement ran after the stated 120-second settlement window.
 
 Cursor's extractor currently reports no LLM-call/tool-call count, so those
-fields must not be treated as zero. mini-swe-agent cost comes from each
-trajectory's `model_stats.instance_cost`.
+fields must not be treated as zero. mini-swe-agent cost comes from summing the
+same OpenRouter `usage.cost` field Daimonos records; LiteLLM's aggregate is
+retained only as a consistency check.
 
-At least two more matched repetitions, plus an explicit cache-policy decision,
-are needed before interpreting relative efficiency.
+At least two more matched repetitions are needed before publishing relative
+efficiency. An explicit cache-policy decision is needed only for causal
+attribution, not for the harness-default cost ranking above.
diff --git i/benchmarks/swebench/README.md w/benchmarks/swebench/README.md
index 54ad763..237ecfe 100644
--- i/benchmarks/swebench/README.md
+++ w/benchmarks/swebench/README.md
@@ -66,9 +66,10 @@ Each run writes `results/<run-id>/` with per-instance token/cost JSONs
 (same schema as the in-house suite — `../analyze.py results/` works),
 `.patch` files, raw transcripts, and `preds.jsonl`.
 mini-swe-agent writes trajectories plus `preds.json`; normalize each trajectory
-through `extract_mini.py` before cross-harness token analysis. Cursor uses its
-own backend, so token/correctness comparisons are available but USD cost parity
-is not.
+through `extract_mini.py` before cross-harness analysis. The normalizer sums
+OpenRouter's per-generation `usage.cost`, the same accounting source Daimonos
+uses; LiteLLM's aggregate is not used for ranking. Cursor uses its own backend,
+so token/correctness comparisons are available but USD cost parity is not.
 
 Delete incomplete smoke directories created before dataset enrichment before
 treating `results/` as a baseline; a valid run contains `preds.jsonl`, a
diff --git i/benchmarks/swebench/extract_mini.py w/benchmarks/swebench/extract_mini.py
index f880e57..c323736 100644
--- i/benchmarks/swebench/extract_mini.py
+++ w/benchmarks/swebench/extract_mini.py
@@ -6,6 +6,7 @@ can be compared with one script.
 Usage: extract_mini.py TRAJ_JSON INSTANCE_ID REPO MODEL OUT_JSON
 """
 import json
+import math
 import sys
 
 
@@ -13,18 +14,50 @@ def main():
     traj_path, iid, repo, model, out_path = sys.argv[1:6]
     t = json.load(open(traj_path))
     tot_in = tot_out = calls = 0
+    costs = []
+    cost_complete = True
     timestamps = []
     for m in t.get("messages", []):
         extra = m.get("extra") or {}
-        u = (extra.get("response") or {}).get("usage")
-        if u:
+        if "response" in extra:
             calls += 1
+            response = extra.get("response")
+            usage = response.get("usage") if isinstance(response, dict) else None
+            if not isinstance(usage, dict) or not usage:
+                cost_complete = False
+                usage = {}
+
+            u = usage
             tot_in += u.get("prompt_tokens", 0) or 0
             tot_out += u.get("completion_tokens", 0) or 0
+            cost = u.get("cost")
+            if (
+                isinstance(cost, (int, float))
+                and not isinstance(cost, bool)
+                and cost >= 0
+            ):
+                costs.append(cost)
+            else:
+                cost_complete = False
         if "timestamp" in extra:
             timestamps.append(extra["timestamp"])
     wall_ms = int((max(timestamps) - min(timestamps)) * 1000) if len(timestamps) > 1 else 0
     exit_status = (t.get("info") or {}).get("exit_status")
+    provider_cost = math.fsum(costs) if calls and cost_complete else None
+    model_stats_cost = (
+        (t.get("info") or {}).get("model_stats") or {}
+    ).get("instance_cost")
+    if not (
+        isinstance(model_stats_cost, (int, float))
+        and not isinstance(model_stats_cost, bool)
+        and model_stats_cost >= 0
+    ):
+        model_stats_cost = None
+    cost_matches_model_stats = (
+        math.isclose(provider_cost, model_stats_cost, rel_tol=1e-9, abs_tol=1e-12)
+        if provider_cost is not None and model_stats_cost is not None
+        else None
+    )
     summary = {
         "task_id": iid,
         "task_name": repo,
@@ -37,7 +70,12 @@ def main():
         "total_tokens": tot_in + tot_out,
         "prompt_tokens": tot_in,
         "llm_calls": calls,
-        "cost_usd": None,
+        # Same accounting source as Daimonos: OpenRouter's per-generation
+        # usage.cost, not mini-swe-agent/LiteLLM's model-price estimate.
+        "cost_usd": provider_cost,
+        "cost_source": "openrouter_usage" if provider_cost is not None else None,
+        "model_stats_cost_usd": model_stats_cost,
+        "cost_matches_model_stats": cost_matches_model_stats,
         "exit_code": 0 if exit_status == "Submitted" else 1,
         "is_error": exit_status != "Submitted",
         "success": exit_status == "Submitted",
diff --git 1/tests/test_swebench_extract_mini.py 2/tests/test_swebench_extract_mini.py
new file mode 100644
index 0000000..e40ee34
--- /dev/null
+++ 2/tests/test_swebench_extract_mini.py
@@ -0,0 +1,206 @@
+"""Tests for mini-swe-agent SWE-bench summary normalization."""
+
+from __future__ import annotations
+
+import json
+import subprocess
+import sys
+from pathlib import Path
+
+import pytest
+
+
+SCRIPT = (
+    Path(__file__).resolve().parents[1] / "benchmarks" / "swebench" / "extract_mini.py"
+)
+
+
+def _extract(tmp_path, payload):
+    trajectory = tmp_path / "trajectory.json"
+    output = tmp_path / "summary.json"
+    trajectory.write_text(json.dumps(payload))
+    subprocess.run(
+        [
+            sys.executable,
+            str(SCRIPT),
+            str(trajectory),
+            "django__django-11815",
+            "django/django",
+            "openrouter/anthropic/claude-opus-4.8",
+            str(output),
+        ],
+        check=True,
+        capture_output=True,
+        text=True,
+        timeout=30,
+    )
+    return json.loads(output.read_text())
+
+
+def test_extract_mini_sums_openrouter_reported_cost(tmp_path):
+    summary = _extract(
+        tmp_path,
+        {
+            "messages": [
+                {
+                    "extra": {
+                        "timestamp": 10.0,
+                        "response": {
+                            "usage": {
+                                "prompt_tokens": 100,
+                                "completion_tokens": 10,
+                                "cost": 0.0125,
+                            }
+                        },
+                    }
+                },
+                {
+                    "extra": {
+                        "timestamp": 12.5,
+                        "response": {
+                            "usage": {
+                                "prompt_tokens": 120,
+                                "completion_tokens": 20,
+                                "cost": 0.0175,
+                            }
+                        },
+                    }
+                },
+            ],
+            "info": {
+                "exit_status": "Submitted",
+                "model_stats": {"instance_cost": 0.03},
+            },
+        },
+    )
+
+    assert summary["total_tokens"] == 250
+    assert summary["llm_calls"] == 2
+    assert summary["cost_usd"] == pytest.approx(0.03)
+    assert summary["cost_source"] == "openrouter_usage"
+    assert summary["model_stats_cost_usd"] == pytest.approx(0.03)
+    assert summary["cost_matches_model_stats"] is True
+    assert summary["wall_ms"] == 2500
+
+
+def test_extract_mini_rejects_partial_provider_cost(tmp_path):
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
+                },
+                {
+                    "extra": {
+                        "response": {
+                            "usage": {
+                                "prompt_tokens": 120,
+                                "completion_tokens": 20,
+                            }
+                        }
+                    }
+                },
+            ],
+            "info": {"exit_status": "Submitted"},
+        },
+    )
+
+    assert summary["cost_usd"] is None
+    assert summary["cost_source"] is None
+
+
+@pytest.mark.parametrize("bad_cost", [True, -0.01])
+def test_extract_mini_rejects_invalid_provider_cost(tmp_path, bad_cost):
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
+                                "cost": bad_cost,
+                            }
+                        }
+                    }
+                }
+            ],
+            "info": {"exit_status": "Submitted"},
+        },
+    )
+
+    assert summary["llm_calls"] == 1
+    assert summary["cost_usd"] is None
+
+
+def test_extract_mini_rejects_generation_without_usage(tmp_path):
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
+                },
+                {"extra": {"response": {}}},
+            ],
+            "info": {"exit_status": "Submitted"},
+        },
+    )
+
+    assert summary["llm_calls"] == 2
+    assert summary["cost_usd"] is None
+
+
+def test_extract_mini_reports_no_cost_without_generations(tmp_path):
+    summary = _extract(
+        tmp_path,
+        {"messages": [{"role": "user", "content": "task"}], "info": {}},
+    )
+
+    assert summary["llm_calls"] == 0
+    assert summary["cost_usd"] is None
+    assert summary["cost_source"] is None
+
+
+def test_extract_mini_flags_model_stats_cost_mismatch(tmp_path):
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
+            "info": {"model_stats": {"instance_cost": 0.02}},
+        },
+    )
+
+    assert summary["model_stats_cost_usd"] == pytest.approx(0.02)
+    assert summary["cost_matches_model_stats"] is False

### New Points
R1. Applied all optional suggestions: the report now calls the n=1 ranking provisional; tests cover negative, boolean, missing-usage, and zero-call paths; math.fsum eliminates sequential-sum drift.

**Requesting:** RE-REVIEW

<!-- event id="review-2" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
None.

### Responses to Prior Points
Re R1: Confirmed. S1 is addressed: the results paragraph now labels the ranking as a 'single, provisional matched repetition'. S2 is addressed via the parametrized invalid-cost test (True and -0.01), the missing-usage test, and the zero-generation test, which together exercise both branches of the cost gate. S3 is addressed with math.fsum accumulation over a costs list. All three suggestions are satisfied as implemented.
B1: resolved
B2: resolved
B3: resolved

**Verdict:** APPROVE
