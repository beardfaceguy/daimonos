# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1464-guard-report" -->

<!-- event id="request" artifact path="1464-guard-report/artifacts/round-1-review-request.diff" sha256="a7310217f5af4f001e5d42777711bac4d29a897b94b5d42392340ea93a93f574" -->
## Review Request — Round 1
**Task:** 1464 — Add report-only SWE-bench guard calibration
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Contract: --guard-mode=report requires positive cost and total-wall thresholds, never terminates or changes an instance, annotates each summary with observed values and >= threshold crossings, and writes a run-level trigger list. analyze_guards.py replays the exact function over existing summary directories and joins evaluator resolved/unresolved ids. Missing cost/correctness remains null/unknown; reports never claim savings.

### Relevant Code / Diff
diff --git i/benchmarks/swebench/README.md w/benchmarks/swebench/README.md
index 55b23f6..967b9d1 100644
--- i/benchmarks/swebench/README.md
+++ w/benchmarks/swebench/README.md
@@ -72,6 +72,27 @@ attempt boundary. Per-instance `wall_ms` includes settlement waits and
 `in_flight_retries` records the retry count. Other HTTP 402 billing failures are
 never retried.
 
+Report-only guard calibration never terminates a task:
+
+```sh
+.venv/bin/python run_agent.py --docker \
+  --guard-mode report --max-instance-cost 2 --max-instance-wall 300 \
+  --instance-ids <ids> --tag guard-calibration
+```
+
+Each instance summary records cost/wall crossings and the run writes
+`guard-summary.json`. Replay the same policy over existing summaries, joining
+an evaluator report when `correct` is not already embedded:
+
+```sh
+.venv/bin/python analyze_guards.py \
+  --cost-limit 2 --wall-limit 300 \
+  --evaluator-report <evaluator.json> results/<run-id>
+```
+
+Report-only crossings are not savings claims; they identify which correct and
+incorrect tasks an enforcing guard would have interrupted.
+
 Each run writes `results/<run-id>/` with per-instance token/cost JSONs
 (same schema as the in-house suite — `../analyze.py results/` works),
 `.patch` files, raw transcripts, and `preds.jsonl`.
diff --git i/benchmarks/swebench/run_agent.py w/benchmarks/swebench/run_agent.py
index f3ed176..aba3993 100644
--- i/benchmarks/swebench/run_agent.py
+++ w/benchmarks/swebench/run_agent.py
@@ -192,6 +192,33 @@ def summarize_tool_traces(summaries, expected):
     }
 
 
+def guard_report(summary, *, cost_limit, wall_limit_seconds):
+    observed_cost = summary.get("cost_usd")
+    cost_would_trigger = (
+        observed_cost >= cost_limit
+        if isinstance(observed_cost, (int, float))
+        and not isinstance(observed_cost, bool)
+        else None
+    )
+    observed_wall = summary.get("wall_ms")
+    wall_limit_ms = wall_limit_seconds * 1000
+    wall_would_trigger = (
+        observed_wall >= wall_limit_ms
+        if isinstance(observed_wall, int) and not isinstance(observed_wall, bool)
+        else None
+    )
+    return {
+        "mode": "report",
+        "cost_limit_usd": cost_limit,
+        "wall_limit_ms": wall_limit_ms,
+        "cost_observed_usd": observed_cost,
+        "wall_observed_ms": observed_wall,
+        "cost_would_trigger": cost_would_trigger,
+        "wall_would_trigger": wall_would_trigger,
+        "would_trigger": cost_would_trigger is True or wall_would_trigger is True,
+    }
+
+
 def in_flight_retry_after(error_text, max_retry_after):
     prefix = "Error: agent error: openrouter 402 Payment Required: "
     for line in error_text.splitlines():
@@ -258,6 +285,9 @@ def run_instance_docker(
     keep,
     in_flight_retries,
     max_retry_after,
+    guard_mode,
+    max_instance_cost,
+    max_instance_wall,
 ):
     iid = inst["instance_id"]
     image = inst.get("image")
@@ -411,6 +441,15 @@ def run_instance_docker(
     data["in_flight_retries"] = retry_count
     data["tool_trace_file"] = trace_path.name if trace_path.exists() else None
     data["tool_trace_rows"] = trace_rows
+    data["guard"] = (
+        guard_report(
+            data,
+            cost_limit=max_instance_cost,
+            wall_limit_seconds=max_instance_wall,
+        )
+        if guard_mode == "report"
+        else None
+    )
     summary.write_text(json.dumps(data, indent=2))
     if not keep:
         shutil.rmtree(benchdir, ignore_errors=True)
@@ -505,6 +544,24 @@ def main():
         default=300,
         help="maximum accepted OpenRouter Retry-After delay in seconds",
     )
+    ap.add_argument(
+        "--guard-mode",
+        choices=("off", "report"),
+        default="off",
+        help="annotate threshold crossings without stopping (report) or disable",
+    )
+    ap.add_argument(
+        "--max-instance-cost",
+        type=float,
+        default=None,
+        help="report-only per-instance cost threshold in USD",
+    )
+    ap.add_argument(
+        "--max-instance-wall",
+        type=int,
+        default=None,
+        help="report-only per-instance total wall threshold in seconds",
+    )
     ap.add_argument("--keep", action="store_true", help="keep worktrees after run")
     ap.add_argument(
         "--docker", action="store_true",
@@ -516,6 +573,13 @@ def main():
         ap.error("--in-flight-retries must be non-negative")
     if args.max_retry_after <= 0:
         ap.error("--max-retry-after must be positive")
+    if args.guard_mode == "report":
+        if args.max_instance_cost is None or args.max_instance_cost <= 0:
+            ap.error("--guard-mode=report requires positive --max-instance-cost")
+        if args.max_instance_wall is None or args.max_instance_wall <= 0:
+            ap.error("--guard-mode=report requires positive --max-instance-wall")
+        if not args.docker:
+            ap.error("--guard-mode=report currently requires --docker")
 
     if not INSTANCES.exists():
         sys.exit("instances.jsonl missing — run fetch_dataset.py first")
@@ -580,6 +644,9 @@ def main():
                         args.keep,
                         args.in_flight_retries,
                         args.max_retry_after,
+                        args.guard_mode,
+                        args.max_instance_cost,
+                        args.max_instance_wall,
                     )
                 else:
                     patch = run_instance(
@@ -604,6 +671,37 @@ def main():
             json.dumps(trace_summary, indent=2) + "\n"
         )
         print(f"Tool traces: {trace_summary}")
+        if args.guard_mode == "report":
+            guard_rows = [
+                (row["task_id"], row["guard"])
+                for row in summaries
+                if row.get("guard")
+            ]
+            guard_summary = {
+                "mode": "report",
+                "cost_limit_usd": args.max_instance_cost,
+                "wall_limit_ms": args.max_instance_wall * 1000,
+                "instances": len(guard_rows),
+                "would_trigger": [
+                    task_id
+                    for task_id, guard in guard_rows
+                    if guard["would_trigger"]
+                ],
+                "cost_would_trigger": [
+                    task_id
+                    for task_id, guard in guard_rows
+                    if guard["cost_would_trigger"] is True
+                ],
+                "wall_would_trigger": [
+                    task_id
+                    for task_id, guard in guard_rows
+                    if guard["wall_would_trigger"] is True
+                ],
+            }
+            (run_dir / "guard-summary.json").write_text(
+                json.dumps(guard_summary, indent=2) + "\n"
+            )
+            print(f"Guard report: {guard_summary}")
 
     print(f"\nPredictions: {preds_path}")
     print("Evaluate with (needs docker):")
diff --git i/tests/test_swebench_runner.py w/tests/test_swebench_runner.py
index e8ea036..e590603 100644
--- i/tests/test_swebench_runner.py
+++ w/tests/test_swebench_runner.py
@@ -271,3 +271,24 @@ def test_tool_trace_summary_exposes_missing_and_unknown_snapshots():
         "unknown_schema": 1,
         "total_rows": 7,
     }
+
+
+def test_guard_report_marks_cost_without_stopping_the_run():
+    runner = load_runner()
+
+    report = runner.guard_report(
+        {"cost_usd": 2.5, "wall_ms": 250_000, "correct": True},
+        cost_limit=2.0,
+        wall_limit_seconds=300,
+    )
+
+    assert report == {
+        "mode": "report",
+        "cost_limit_usd": 2.0,
+        "wall_limit_ms": 300_000,
+        "cost_observed_usd": 2.5,
+        "wall_observed_ms": 250_000,
+        "cost_would_trigger": True,
+        "wall_would_trigger": False,
+        "would_trigger": True,
+    }
diff --git 1/benchmarks/swebench/analyze_guards.py 2/benchmarks/swebench/analyze_guards.py
new file mode 100644
index 0000000..f82e9db
--- /dev/null
+++ 2/benchmarks/swebench/analyze_guards.py
@@ -0,0 +1,101 @@
+#!/usr/bin/env python3
+"""Replay report-only per-instance guard thresholds over summary directories."""
+
+import argparse
+import json
+import pathlib
+
+import run_agent
+
+
+def main():
+    parser = argparse.ArgumentParser()
+    parser.add_argument("--cost-limit", type=float, required=True)
+    parser.add_argument("--wall-limit", type=int, required=True)
+    parser.add_argument(
+        "--evaluator-report",
+        action="append",
+        default=[],
+        type=pathlib.Path,
+    )
+    parser.add_argument("run_dirs", nargs="+", type=pathlib.Path)
+    args = parser.parse_args()
+    if args.cost_limit <= 0:
+        parser.error("--cost-limit must be positive")
+    if args.wall_limit <= 0:
+        parser.error("--wall-limit must be positive")
+
+    rows = {}
+    for run_dir in args.run_dirs:
+        for path in sorted(run_dir.glob("*.json")):
+            try:
+                row = json.loads(path.read_text())
+            except (OSError, ValueError):
+                continue
+            task_id = row.get("task_id")
+            if not task_id:
+                continue
+            if task_id in rows:
+                parser.error(f"duplicate task summary: {task_id}")
+            rows[task_id] = row
+
+    correctness = {}
+    for path in args.evaluator_report:
+        report = json.loads(path.read_text())
+        for task_id in report.get("resolved_ids", []):
+            correctness[task_id] = True
+        for task_id in report.get("unresolved_ids", []):
+            correctness[task_id] = False
+    for task_id, row in rows.items():
+        if isinstance(row.get("correct"), bool):
+            correctness[task_id] = row["correct"]
+
+    reports = {
+        task_id: run_agent.guard_report(
+            row,
+            cost_limit=args.cost_limit,
+            wall_limit_seconds=args.wall_limit,
+        )
+        for task_id, row in rows.items()
+    }
+    triggered = sorted(
+        task_id for task_id, report in reports.items() if report["would_trigger"]
+    )
+    output = {
+        "cost_limit_usd": args.cost_limit,
+        "wall_limit_ms": args.wall_limit * 1000,
+        "instances": len(rows),
+        "observed_cost_usd": sum(
+            row["cost_usd"]
+            for row in rows.values()
+            if isinstance(row.get("cost_usd"), (int, float))
+            and not isinstance(row["cost_usd"], bool)
+        ),
+        "would_trigger": triggered,
+        "cost_would_trigger": sorted(
+            task_id
+            for task_id, report in reports.items()
+            if report["cost_would_trigger"] is True
+        ),
+        "wall_would_trigger": sorted(
+            task_id
+            for task_id, report in reports.items()
+            if report["wall_would_trigger"] is True
+        ),
+        "correct_would_trigger": sorted(
+            task_id for task_id in triggered if correctness.get(task_id) is True
+        ),
+        "incorrect_would_trigger": sorted(
+            task_id for task_id in triggered if correctness.get(task_id) is False
+        ),
+        "unknown_correctness_would_trigger": sorted(
+            task_id
+            for task_id in triggered
+            if not isinstance(correctness.get(task_id), bool)
+        ),
+    }
+    print(json.dumps(output, indent=2))
+
+
+if __name__ == "__main__":
+    main()
diff --git 1/tests/test_swebench_guards.py 2/tests/test_swebench_guards.py
new file mode 100644
index 0000000..08d6836
--- /dev/null
+++ 2/tests/test_swebench_guards.py
@@ -0,0 +1,66 @@
+"""Tests for SWE-bench guard calibration reports."""
+
+from __future__ import annotations
+
+import json
+import subprocess
+import sys
+from pathlib import Path
+
+
+SCRIPT = (
+    Path(__file__).resolve().parents[1]
+    / "benchmarks"
+    / "swebench"
+    / "analyze_guards.py"
+)
+
+
+def test_guard_analysis_reports_correctness_tradeoff(tmp_path):
+    (tmp_path / "a.json").write_text(
+        json.dumps(
+            {
+                "task_id": "a",
+                "cost_usd": 2.5,
+                "wall_ms": 250_000,
+            }
+        )
+    )
+    (tmp_path / "b.json").write_text(
+        json.dumps(
+            {
+                "task_id": "b",
+                "cost_usd": 1.0,
+                "wall_ms": 350_000,
+            }
+        )
+    )
+    evaluator = tmp_path / "evaluator.json"
+    evaluator.write_text(
+        json.dumps({"resolved_ids": ["a"], "unresolved_ids": ["b"]})
+    )
+
+    completed = subprocess.run(
+        [
+            sys.executable,
+            str(SCRIPT),
+            "--cost-limit",
+            "2",
+            "--wall-limit",
+            "300",
+            "--evaluator-report",
+            str(evaluator),
+            str(tmp_path),
+        ],
+        check=True,
+        capture_output=True,
+        text=True,
+        timeout=30,
+    )
+
+    report = json.loads(completed.stdout)
+    assert report["instances"] == 2
+    assert report["observed_cost_usd"] == 3.5
+    assert report["would_trigger"] == ["a", "b"]
+    assert report["correct_would_trigger"] == ["a"]
+    assert report["incorrect_would_trigger"] == ["b"]

### Known Concerns
1. Report-only total wall is measured after completion; it calibrates policy but does not model exact kill timing.
2. Cost is provider-reported after each response, so a future enforcing guard can overshoot by its final response.
3. The initial $2/300s replay interrupts two correct full-run outliers; final policy requires repeated calibration.

### Specific Questions for Reviewer
1. Does report-only mode provably leave execution unchanged?
2. Are threshold boundary, unknown-value, evaluator-join, and aggregate semantics correct?
3. Is this sufficient evidence before targeted paid calibration?
4. Are CLI validation and artifacts reproducible?

<!-- event id="review-1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. analyze_guards.py's summary-directory glob (`run_dir.glob("*.json")`) will ingest non-instance JSON artifacts written to the same run directory — the diff itself shows `guard-summary.json` and a tool-trace summary are written into `results/<run-id>/`, and preds/evaluator files may also live there. Any such file that happens to carry a `task_id` key would be treated as an instance row, and a copied evaluator report inside the run dir could silently skew counts. Filter to the known per-instance summary schema (or an explicit filename pattern / schema check) rather than every *.json in the directory, and add a test with a guard-summary.json present in the scanned directory.
B2. guard_report treats float wall values as unknown: `wall_would_trigger` requires `isinstance(observed_wall, int)`, so a summary with `wall_ms` as a float (e.g. 250000.0, which is what json round-trips of computed durations commonly produce) yields None instead of a real comparison, silently dropping instances from wall trigger lists. Accept int and float (excluding bool) symmetrically with the cost check, and add a test for float wall_ms. This directly affects the 'unknown-value semantics correct?' question — the current behavior conflates 'missing' with 'float-typed'.

### Non-Blocking Suggestions
S1. In analyze_guards.py, embedded per-row `correct` unconditionally overrides evaluator-report correctness. If they conflict, the tool silently prefers the embedded value. Consider detecting and erroring (or warning) on disagreement between an embedded `correct` and the evaluator join, since a stale embedded flag would corrupt the correct/incorrect trigger split.
S2. The run-level guard summary in run_agent.py builds `guard_rows` only from rows where `row.get("guard")` is truthy, so failed/errored instances (or rows without a guard dict) are silently excluded and `instances` undercounts the run. Consider reporting a separate count of instances lacking guard data so calibration coverage is visible.
S3. There is no unit test for guard_report's unknown-value path (cost_usd/wall_ms missing or null) asserting cost_would_trigger/wall_would_trigger are None and would_trigger is False, nor a boundary test at exactly the threshold (>= semantics). Given the request explicitly asks about boundary and unknown-value semantics, add both tests.
S4. README documents `analyze_guards.py --cost-limit 2 --wall-limit 300 results/<run-id>` but the script imports run_agent by bare module name, so it only works when invoked with CWD (or sys.path) at benchmarks/swebench. Either document the required working directory or make the import robust (e.g. add the script's directory to sys.path).
S5. On the evidence question: report-only mode plausibly leaves execution unchanged (guard_report is a pure post-hoc annotation and the diff adds no control-flow into the run path), and the replay design is sound. Given the known concern that the initial $2/300s policy interrupts two correct outliers, the evidence supports proceeding to targeted paid calibration only after the fixes above, since the wall-trigger typing bug (B2) could be understating wall crossings in the current replay.

**Verdict:** REVISE

<!-- event id="rebuttal-1" artifact path="1464-guard-report/artifacts/round-1-rebuttal.diff" sha256="1d601bdd32f53cd120fea8fbecec2bb60570d2b0ac48defa6b679e3d35024cff" -->
## Rebuttal — Round 1
**Position:** AGREE

### Responses to Blocking Concerns
Re B1: ACCEPT — Replay now accepts only files named exactly <task_id>.json; a guard-summary.json carrying a bogus task_id is present in the integration test and excluded.
Re B2: ACCEPT — wall_ms now accepts int or float while excluding bool, symmetric with cost. Exact float threshold behavior is tested.

### Revised Code / Diff
diff --git i/benchmarks/swebench/README.md w/benchmarks/swebench/README.md
index 55b23f6..5057254 100644
--- i/benchmarks/swebench/README.md
+++ w/benchmarks/swebench/README.md
@@ -72,6 +72,28 @@ attempt boundary. Per-instance `wall_ms` includes settlement waits and
 `in_flight_retries` records the retry count. Other HTTP 402 billing failures are
 never retried.
 
+Report-only guard calibration never terminates a task:
+
+```sh
+.venv/bin/python run_agent.py --docker \
+  --guard-mode report --max-instance-cost 2 --max-instance-wall 300 \
+  --instance-ids <ids> --tag guard-calibration
+```
+
+Each instance summary records cost/wall crossings and the run writes
+`guard-summary.json`. Replay the same policy over existing summaries, joining
+an evaluator report when `correct` is not already embedded. Run from
+`benchmarks/swebench/`:
+
+```sh
+.venv/bin/python analyze_guards.py \
+  --cost-limit 2 --wall-limit 300 \
+  --evaluator-report <evaluator.json> results/<run-id>
+```
+
+Report-only crossings are not savings claims; they identify which correct and
+incorrect tasks an enforcing guard would have interrupted.
+
 Each run writes `results/<run-id>/` with per-instance token/cost JSONs
 (same schema as the in-house suite — `../analyze.py results/` works),
 `.patch` files, raw transcripts, and `preds.jsonl`.
diff --git i/benchmarks/swebench/run_agent.py w/benchmarks/swebench/run_agent.py
index f3ed176..f29966b 100644
--- i/benchmarks/swebench/run_agent.py
+++ w/benchmarks/swebench/run_agent.py
@@ -192,6 +192,62 @@ def summarize_tool_traces(summaries, expected):
     }
 
 
+def guard_report(summary, *, cost_limit, wall_limit_seconds):
+    observed_cost = summary.get("cost_usd")
+    cost_would_trigger = (
+        observed_cost >= cost_limit
+        if isinstance(observed_cost, (int, float))
+        and not isinstance(observed_cost, bool)
+        else None
+    )
+    observed_wall = summary.get("wall_ms")
+    wall_limit_ms = wall_limit_seconds * 1000
+    wall_would_trigger = (
+        observed_wall >= wall_limit_ms
+        if isinstance(observed_wall, (int, float))
+        and not isinstance(observed_wall, bool)
+        else None
+    )
+    return {
+        "mode": "report",
+        "cost_limit_usd": cost_limit,
+        "wall_limit_ms": wall_limit_ms,
+        "cost_observed_usd": observed_cost,
+        "wall_observed_ms": observed_wall,
+        "cost_would_trigger": cost_would_trigger,
+        "wall_would_trigger": wall_would_trigger,
+        "would_trigger": cost_would_trigger is True or wall_would_trigger is True,
+    }
+
+
+def summarize_guard_reports(summaries):
+    guard_rows = [
+        (row["task_id"], row["guard"])
+        for row in summaries
+        if isinstance(row.get("guard"), dict)
+    ]
+    return {
+        "instances": len(summaries),
+        "reported": len(guard_rows),
+        "missing_guard_data": len(summaries) - len(guard_rows),
+        "would_trigger": [
+            task_id
+            for task_id, guard in guard_rows
+            if guard["would_trigger"]
+        ],
+        "cost_would_trigger": [
+            task_id
+            for task_id, guard in guard_rows
+            if guard["cost_would_trigger"] is True
+        ],
+        "wall_would_trigger": [
+            task_id
+            for task_id, guard in guard_rows
+            if guard["wall_would_trigger"] is True
+        ],
+    }
+
+
 def in_flight_retry_after(error_text, max_retry_after):
     prefix = "Error: agent error: openrouter 402 Payment Required: "
     for line in error_text.splitlines():
@@ -258,6 +314,9 @@ def run_instance_docker(
     keep,
     in_flight_retries,
     max_retry_after,
+    guard_mode,
+    max_instance_cost,
+    max_instance_wall,
 ):
     iid = inst["instance_id"]
     image = inst.get("image")
@@ -411,6 +470,15 @@ def run_instance_docker(
     data["in_flight_retries"] = retry_count
     data["tool_trace_file"] = trace_path.name if trace_path.exists() else None
     data["tool_trace_rows"] = trace_rows
+    data["guard"] = (
+        guard_report(
+            data,
+            cost_limit=max_instance_cost,
+            wall_limit_seconds=max_instance_wall,
+        )
+        if guard_mode == "report"
+        else None
+    )
     summary.write_text(json.dumps(data, indent=2))
     if not keep:
         shutil.rmtree(benchdir, ignore_errors=True)
@@ -505,6 +573,24 @@ def main():
         default=300,
         help="maximum accepted OpenRouter Retry-After delay in seconds",
     )
+    ap.add_argument(
+        "--guard-mode",
+        choices=("off", "report"),
+        default="off",
+        help="annotate threshold crossings without stopping (report) or disable",
+    )
+    ap.add_argument(
+        "--max-instance-cost",
+        type=float,
+        default=None,
+        help="report-only per-instance cost threshold in USD",
+    )
+    ap.add_argument(
+        "--max-instance-wall",
+        type=int,
+        default=None,
+        help="report-only per-instance total wall threshold in seconds",
+    )
     ap.add_argument("--keep", action="store_true", help="keep worktrees after run")
     ap.add_argument(
         "--docker", action="store_true",
@@ -516,6 +602,13 @@ def main():
         ap.error("--in-flight-retries must be non-negative")
     if args.max_retry_after <= 0:
         ap.error("--max-retry-after must be positive")
+    if args.guard_mode == "report":
+        if args.max_instance_cost is None or args.max_instance_cost <= 0:
+            ap.error("--guard-mode=report requires positive --max-instance-cost")
+        if args.max_instance_wall is None or args.max_instance_wall <= 0:
+            ap.error("--guard-mode=report requires positive --max-instance-wall")
+        if not args.docker:
+            ap.error("--guard-mode=report currently requires --docker")
 
     if not INSTANCES.exists():
         sys.exit("instances.jsonl missing — run fetch_dataset.py first")
@@ -580,6 +673,9 @@ def main():
                         args.keep,
                         args.in_flight_retries,
                         args.max_retry_after,
+                        args.guard_mode,
+                        args.max_instance_cost,
+                        args.max_instance_wall,
                     )
                 else:
                     patch = run_instance(
@@ -604,6 +700,17 @@ def main():
             json.dumps(trace_summary, indent=2) + "\n"
         )
         print(f"Tool traces: {trace_summary}")
+        if args.guard_mode == "report":
+            guard_summary = summarize_guard_reports(summaries)
+            guard_summary.update({
+                "mode": "report",
+                "cost_limit_usd": args.max_instance_cost,
+                "wall_limit_ms": args.max_instance_wall * 1000,
+            })
+            (run_dir / "guard-summary.json").write_text(
+                json.dumps(guard_summary, indent=2) + "\n"
+            )
+            print(f"Guard report: {guard_summary}")
 
     print(f"\nPredictions: {preds_path}")
     print("Evaluate with (needs docker):")
diff --git i/tests/test_swebench_runner.py w/tests/test_swebench_runner.py
index e8ea036..beebb24 100644
--- i/tests/test_swebench_runner.py
+++ w/tests/test_swebench_runner.py
@@ -271,3 +271,75 @@ def test_tool_trace_summary_exposes_missing_and_unknown_snapshots():
         "unknown_schema": 1,
         "total_rows": 7,
     }
+
+
+def test_guard_report_marks_cost_without_stopping_the_run():
+    runner = load_runner()
+
+    report = runner.guard_report(
+        {"cost_usd": 2.5, "wall_ms": 250_000, "correct": True},
+        cost_limit=2.0,
+        wall_limit_seconds=300,
+    )
+
+    assert report == {
+        "mode": "report",
+        "cost_limit_usd": 2.0,
+        "wall_limit_ms": 300_000,
+        "cost_observed_usd": 2.5,
+        "wall_observed_ms": 250_000,
+        "cost_would_trigger": True,
+        "wall_would_trigger": False,
+        "would_trigger": True,
+    }
+
+
+def test_guard_report_accepts_float_wall_at_exact_boundaries():
+    runner = load_runner()
+
+    report = runner.guard_report(
+        {"cost_usd": 2.0, "wall_ms": 300_000.0},
+        cost_limit=2.0,
+        wall_limit_seconds=300,
+    )
+
+    assert report["cost_would_trigger"] is True
+    assert report["wall_would_trigger"] is True
+    assert report["would_trigger"] is True
+
+
+def test_guard_report_preserves_unknown_observations():
+    runner = load_runner()
+
+    report = runner.guard_report(
+        {"cost_usd": None, "wall_ms": None},
+        cost_limit=2.0,
+        wall_limit_seconds=300,
+    )
+
+    assert report["cost_would_trigger"] is None
+    assert report["wall_would_trigger"] is None
+    assert report["would_trigger"] is False
+
+
+def test_guard_summary_exposes_missing_report_coverage():
+    runner = load_runner()
+
+    summary = runner.summarize_guard_reports(
+        [
+            {
+                "task_id": "a",
+                "guard": {
+                    "would_trigger": True,
+                    "cost_would_trigger": True,
+                    "wall_would_trigger": False,
+                },
+            },
+            {"task_id": "b", "guard": None},
+        ]
+    )
+
+    assert summary["instances"] == 2
+    assert summary["reported"] == 1
+    assert summary["missing_guard_data"] == 1
+    assert summary["would_trigger"] == ["a"]
diff --git 1/benchmarks/swebench/analyze_guards.py 2/benchmarks/swebench/analyze_guards.py
new file mode 100644
index 0000000..a206d40
--- /dev/null
+++ 2/benchmarks/swebench/analyze_guards.py
@@ -0,0 +1,108 @@
+#!/usr/bin/env python3
+"""Replay report-only per-instance guard thresholds over summary directories."""
+
+import argparse
+import json
+import pathlib
+
+import run_agent
+
+
+def main():
+    parser = argparse.ArgumentParser()
+    parser.add_argument("--cost-limit", type=float, required=True)
+    parser.add_argument("--wall-limit", type=int, required=True)
+    parser.add_argument(
+        "--evaluator-report",
+        action="append",
+        default=[],
+        type=pathlib.Path,
+    )
+    parser.add_argument("run_dirs", nargs="+", type=pathlib.Path)
+    args = parser.parse_args()
+    if args.cost_limit <= 0:
+        parser.error("--cost-limit must be positive")
+    if args.wall_limit <= 0:
+        parser.error("--wall-limit must be positive")
+
+    rows = {}
+    for run_dir in args.run_dirs:
+        for path in sorted(run_dir.glob("*.json")):
+            try:
+                row = json.loads(path.read_text())
+            except (OSError, ValueError):
+                continue
+            task_id = row.get("task_id")
+            if not task_id:
+                continue
+            if path.name != f"{task_id}.json":
+                continue
+            if task_id in rows:
+                parser.error(f"duplicate task summary: {task_id}")
+            rows[task_id] = row
+
+    correctness = {}
+    for path in args.evaluator_report:
+        report = json.loads(path.read_text())
+        for task_id in report.get("resolved_ids", []):
+            correctness[task_id] = True
+        for task_id in report.get("unresolved_ids", []):
+            correctness[task_id] = False
+    for task_id, row in rows.items():
+        if isinstance(row.get("correct"), bool):
+            if (
+                task_id in correctness
+                and correctness[task_id] != row["correct"]
+            ):
+                parser.error(f"correctness conflict for {task_id}")
+            correctness[task_id] = row["correct"]
+
+    reports = {
+        task_id: run_agent.guard_report(
+            row,
+            cost_limit=args.cost_limit,
+            wall_limit_seconds=args.wall_limit,
+        )
+        for task_id, row in rows.items()
+    }
+    triggered = sorted(
+        task_id for task_id, report in reports.items() if report["would_trigger"]
+    )
+    output = {
+        "cost_limit_usd": args.cost_limit,
+        "wall_limit_ms": args.wall_limit * 1000,
+        "instances": len(rows),
+        "observed_cost_usd": sum(
+            row["cost_usd"]
+            for row in rows.values()
+            if isinstance(row.get("cost_usd"), (int, float))
+            and not isinstance(row["cost_usd"], bool)
+        ),
+        "would_trigger": triggered,
+        "cost_would_trigger": sorted(
+            task_id
+            for task_id, report in reports.items()
+            if report["cost_would_trigger"] is True
+        ),
+        "wall_would_trigger": sorted(
+            task_id
+            for task_id, report in reports.items()
+            if report["wall_would_trigger"] is True
+        ),
+        "correct_would_trigger": sorted(
+            task_id for task_id in triggered if correctness.get(task_id) is True
+        ),
+        "incorrect_would_trigger": sorted(
+            task_id for task_id in triggered if correctness.get(task_id) is False
+        ),
+        "unknown_correctness_would_trigger": sorted(
+            task_id
+            for task_id in triggered
+            if not isinstance(correctness.get(task_id), bool)
+        ),
+    }
+    print(json.dumps(output, indent=2))
+
+
+if __name__ == "__main__":
+    main()
diff --git 1/tests/test_swebench_guards.py 2/tests/test_swebench_guards.py
new file mode 100644
index 0000000..b37f53c
--- /dev/null
+++ 2/tests/test_swebench_guards.py
@@ -0,0 +1,110 @@
+"""Tests for SWE-bench guard calibration reports."""
+
+from __future__ import annotations
+
+import json
+import subprocess
+import sys
+from pathlib import Path
+
+
+SCRIPT = (
+    Path(__file__).resolve().parents[1]
+    / "benchmarks"
+    / "swebench"
+    / "analyze_guards.py"
+)
+
+
+def test_guard_analysis_reports_correctness_tradeoff(tmp_path):
+    (tmp_path / "a.json").write_text(
+        json.dumps(
+            {
+                "task_id": "a",
+                "cost_usd": 2.5,
+                "wall_ms": 250_000,
+            }
+        )
+    )
+    (tmp_path / "b.json").write_text(
+        json.dumps(
+            {
+                "task_id": "b",
+                "cost_usd": 1.0,
+                "wall_ms": 350_000,
+            }
+        )
+    )
+    evaluator = tmp_path / "evaluator.json"
+    evaluator.write_text(
+        json.dumps({"resolved_ids": ["a"], "unresolved_ids": ["b"]})
+    )
+    (tmp_path / "guard-summary.json").write_text(
+        json.dumps(
+            {
+                "task_id": "not-an-instance-summary",
+                "cost_usd": 99,
+                "wall_ms": 999_000,
+            }
+        )
+    )
+
+    completed = subprocess.run(
+        [
+            sys.executable,
+            str(SCRIPT),
+            "--cost-limit",
+            "2",
+            "--wall-limit",
+            "300",
+            "--evaluator-report",
+            str(evaluator),
+            str(tmp_path),
+        ],
+        check=True,
+        capture_output=True,
+        text=True,
+        timeout=30,
+    )
+
+    report = json.loads(completed.stdout)
+    assert report["instances"] == 2
+    assert report["observed_cost_usd"] == 3.5
+    assert report["would_trigger"] == ["a", "b"]
+    assert report["correct_would_trigger"] == ["a"]
+    assert report["incorrect_would_trigger"] == ["b"]
+
+
+def test_guard_analysis_rejects_conflicting_correctness(tmp_path):
+    (tmp_path / "a.json").write_text(
+        json.dumps(
+            {
+                "task_id": "a",
+                "cost_usd": 2.5,
+                "wall_ms": 250_000,
+                "correct": False,
+            }
+        )
+    )
+    evaluator = tmp_path / "evaluator.json"
+    evaluator.write_text(json.dumps({"resolved_ids": ["a"]}))
+
+    completed = subprocess.run(
+        [
+            sys.executable,
+            str(SCRIPT),
+            "--cost-limit",
+            "2",
+            "--wall-limit",
+            "300",
+            "--evaluator-report",
+            str(evaluator),
+            str(tmp_path),
+        ],
+        capture_output=True,
+        text=True,
+        timeout=30,
+    )
+
+    assert completed.returncode == 2
+    assert "correctness conflict for a" in completed.stderr

### New Points
R1. Applied all suggestions: evaluator/embedded correctness conflicts hard-error; run summary exposes reported and missing_guard_data counts; unknown and exact-boundary paths are tested; README explicitly requires benchmarks/swebench working directory.

**Requesting:** RE-REVIEW

<!-- event id="review-2" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S6. Minor, non-blocking: the B1 fix (`path.name != f"{task_id}.json"`) is a filename-consistency check rather than a schema check; a stray artifact whose filename happens to match its embedded task_id would still be ingested. This is acceptable given run-directory conventions, and the integration test with a mismatched guard-summary.json covers the realistic case. Consider a lightweight schema check (e.g. require cost_usd or wall_ms keys) if run directories ever accumulate other per-task JSON artifacts.

### Responses to Prior Points
Re R1: Verified against the revised diff. S1: analyze_guards.py now hard-errors on evaluator/embedded correctness conflicts (`parser.error(f"correctness conflict for {task_id}")`) and test_guard_analysis_rejects_conflicting_correctness asserts exit code 2 with the expected stderr message. S2: summarize_guard_reports is factored out and exposes instances/reported/missing_guard_data, with a dedicated unit test covering a row whose guard is None. S3: exact-boundary (>=) semantics for both cost and float wall at the threshold, and the unknown-value path (None cost/wall yielding None triggers and would_trigger False) are both unit-tested. S4: README now states the replay must be run from benchmarks/swebench/, matching the bare `import run_agent`. All accepted dispositions are substantiated in the diff.
B1: resolved
B2: resolved

**Verdict:** APPROVE
