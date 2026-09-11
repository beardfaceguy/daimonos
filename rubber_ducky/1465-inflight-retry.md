# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1465-inflight-retry" -->

<!-- event id="request" artifact path="1465-inflight-retry/artifacts/round-1-review-request.diff" sha256="85f9d0b4d1118a36500bec90e6d5225c905ce3c54fb3f8516c7c71f905043cb0" -->
## Review Request — Round 1
**Task:** 1465 — Retry OpenRouter in-flight-budget settlement in SWE-bench
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Contract: when the Docker runner receives the exact OpenRouter in_flight_budget_exhausted reason plus a positive Retry-After no greater than the configured cap, wait that provider delay and retry the same agent invocation/container up to --in-flight-retries. Preserve each attempt stderr and include waiting in wall time. Never retry ordinary insufficient-credit 402s, missing delays, excessive delays, success, or exhausted budgets.

### Relevant Code / Diff
diff --git i/benchmarks/swebench/README.md w/benchmarks/swebench/README.md
index 44a610f..1a6ac37 100644
--- i/benchmarks/swebench/README.md
+++ w/benchmarks/swebench/README.md
@@ -62,6 +62,11 @@ OPENROUTER_API_KEY=... .venv/bin/mini-extra swebench \
   --environment-class docker --workers 1 --output results/mini-<label>
 ```
 
+Docker runs retry OpenRouter's exact `in_flight_budget_exhausted` response once,
+after the response's own `Retry-After` delay. `--in-flight-retries` changes the
+attempt bound; `--max-retry-after` rejects unexpectedly long provider delays.
+Other HTTP 402 billing failures are never retried.
+
 Each run writes `results/<run-id>/` with per-instance token/cost JSONs
 (same schema as the in-house suite — `../analyze.py results/` works),
 `.patch` files, raw transcripts, and `preds.jsonl`.
diff --git i/benchmarks/swebench/run_agent.py w/benchmarks/swebench/run_agent.py
index d0ea5d2..dbd202f 100644
--- i/benchmarks/swebench/run_agent.py
+++ w/benchmarks/swebench/run_agent.py
@@ -19,10 +19,12 @@ import datetime as dt
 import json
 import os
 import pathlib
+import re
 import shutil
 import subprocess
 import sys
 import tempfile
+import time
 
 HERE = pathlib.Path(__file__).parent
 BENCH_ROOT = HERE.parent
@@ -149,7 +151,49 @@ def docker(args, **kw):
     return run_text(["docker", *args], **kw)
 
 
-def run_instance_docker(inst, run_dir, bench_env, model, timeout, keep):
+def in_flight_retry_after(error_text, max_retry_after):
+    if not re.search(r'"reason"\s*:\s*"in_flight_budget_exhausted"', error_text):
+        return None
+    match = re.search(r'"Retry-After"\s*:\s*"?(\d+)"?', error_text)
+    if not match:
+        return None
+    delay = int(match.group(1))
+    return delay if 0 < delay <= max_retry_after else None
+
+
+def run_with_in_flight_retry(
+    run_once,
+    *,
+    max_retries,
+    max_retry_after,
+    sleep=time.sleep,
+):
+    attempt = 0
+    while True:
+        completed, error_text = run_once(attempt)
+        if completed.returncode == 0 or attempt >= max_retries:
+            return completed
+        delay = in_flight_retry_after(error_text, max_retry_after)
+        if delay is None:
+            return completed
+        attempt += 1
+        print(
+            "       WARN: OpenRouter in-flight budget is settling; "
+            f"retrying in {delay}s ({attempt}/{max_retries})"
+        )
+        sleep(delay)
+
+
+def run_instance_docker(
+    inst,
+    run_dir,
+    bench_env,
+    model,
+    timeout,
+    keep,
+    in_flight_retries,
+    max_retry_after,
+):
     iid = inst["instance_id"]
     image = inst.get("image")
     if not image:
@@ -185,16 +229,35 @@ def run_instance_docker(inst, run_dir, bench_env, model, timeout, keep):
     started = utcnow()
     t0 = dt.datetime.now()
     try:
-        with raw.open("w") as out, err.open("w") as errf:
-            proc = subprocess.run(
-                [
-                    "timeout", "--kill-after=10s", str(timeout),
-                    "docker", "exec", "-w", "/testbed",
-                    "-e", f"BENCH_MODEL={model}",
-                    cname, "bash", "-c", DOCKER_EXEC_SCRIPT,
-                ],
-                stdout=out, stderr=errf, check=False,
-            )
+        def run_once(attempt):
+            mode = "w" if attempt == 0 else "a"
+            with raw.open(mode) as out:
+                completed = subprocess.run(
+                    [
+                        "timeout", "--kill-after=10s", str(timeout),
+                        "docker", "exec", "-w", "/testbed",
+                        "-e", f"BENCH_MODEL={model}",
+                        cname, "bash", "-c", DOCKER_EXEC_SCRIPT,
+                    ],
+                    stdout=out,
+                    stderr=subprocess.PIPE,
+                    text=True,
+                    encoding="utf-8",
+                    errors="replace",
+                    check=False,
+                )
+            current_error = completed.stderr or ""
+            with err.open(mode) as errf:
+                if attempt:
+                    errf.write(f"\n--- in-flight budget retry {attempt} ---\n")
+                errf.write(current_error)
+            return completed, current_error
+
+        proc = run_with_in_flight_retry(
+            run_once,
+            max_retries=in_flight_retries,
+            max_retry_after=max_retry_after,
+        )
         proc_rc = proc.returncode
         if proc_rc in (124, 137):
             print(f"       WARN: {iid} hit timeout ({timeout}s) — killed")
@@ -315,6 +378,18 @@ def main():
     ap.add_argument("--tag", default="", help="label folded into run dir name")
     ap.add_argument("--model", default=None, help="override agent.env model")
     ap.add_argument("--timeout", type=int, default=900, help="per-instance seconds")
+    ap.add_argument(
+        "--in-flight-retries",
+        type=int,
+        default=1,
+        help="retries after OpenRouter in-flight-budget settlement responses",
+    )
+    ap.add_argument(
+        "--max-retry-after",
+        type=int,
+        default=300,
+        help="maximum accepted OpenRouter Retry-After delay in seconds",
+    )
     ap.add_argument("--keep", action="store_true", help="keep worktrees after run")
     ap.add_argument(
         "--docker", action="store_true",
@@ -322,6 +397,10 @@ def main():
              "(runnable test env; requires the musl daimonos build)",
     )
     args = ap.parse_args()
+    if args.in_flight_retries < 0:
+        ap.error("--in-flight-retries must be non-negative")
+    if args.max_retry_after <= 0:
+        ap.error("--max-retry-after must be positive")
 
     if not INSTANCES.exists():
         sys.exit("instances.jsonl missing — run fetch_dataset.py first")
@@ -374,12 +453,23 @@ def main():
 
     preds_path = run_dir / "preds.jsonl"
     try:
-        runner = run_instance_docker if args.docker else run_instance
         with preds_path.open("w") as preds:
             for inst in instances:
-                patch = runner(
-                    inst, run_dir, bench_env, model, args.timeout, args.keep
-                )
+                if args.docker:
+                    patch = run_instance_docker(
+                        inst,
+                        run_dir,
+                        bench_env,
+                        model,
+                        args.timeout,
+                        args.keep,
+                        args.in_flight_retries,
+                        args.max_retry_after,
+                    )
+                else:
+                    patch = run_instance(
+                        inst, run_dir, bench_env, model, args.timeout, args.keep
+                    )
                 preds.write(json.dumps({
                     "instance_id": inst["instance_id"],
                     "model_name_or_path": f"daimonos-{model}",
diff --git i/tests/test_swebench_runner.py w/tests/test_swebench_runner.py
index 301279d..e8b329a 100644
--- i/tests/test_swebench_runner.py
+++ w/tests/test_swebench_runner.py
@@ -7,6 +7,8 @@ import subprocess
 import sys
 from pathlib import Path
 
+import pytest
+
 
 RUNNER = (
     Path(__file__).resolve().parents[1] / "benchmarks" / "swebench" / "run_agent.py"
@@ -56,3 +58,65 @@ def test_collect_patch_encodes_binary_files(tmp_path):
 
     assert "GIT binary patch" in patch
     assert "\ufffd" not in patch
+
+
+def test_in_flight_budget_retries_after_provider_settlement_delay():
+    runner = load_runner()
+    responses = iter(
+        [
+            (
+                subprocess.CompletedProcess([], 1),
+                'openrouter 402: {"metadata":{"reason":'
+                '"in_flight_budget_exhausted","headers":{"Retry-After":"120"}}}',
+            ),
+            (subprocess.CompletedProcess([], 0), ""),
+        ]
+    )
+    attempts = []
+    sleeps = []
+
+    def run_once(attempt):
+        attempts.append(attempt)
+        return next(responses)
+
+    completed = runner.run_with_in_flight_retry(
+        run_once,
+        max_retries=1,
+        max_retry_after=300,
+        sleep=sleeps.append,
+    )
+
+    assert completed.returncode == 0
+    assert attempts == [0, 1]
+    assert sleeps == [120]
+
+
+@pytest.mark.parametrize(
+    "error_text",
+    [
+        'openrouter 402: {"message":"insufficient credits",'
+        '"headers":{"Retry-After":"120"}}',
+        'openrouter 402: {"reason":"in_flight_budget_exhausted"}',
+        'openrouter 402: {"reason":"in_flight_budget_exhausted",'
+        '"headers":{"Retry-After":"999"}}',
+    ],
+)
+def test_other_billing_errors_and_unsafe_delays_are_not_retried(error_text):
+    runner = load_runner()
+    attempts = []
+    sleeps = []
+
+    def run_once(attempt):
+        attempts.append(attempt)
+        return subprocess.CompletedProcess([], 1), error_text
+
+    completed = runner.run_with_in_flight_retry(
+        run_once,
+        max_retries=1,
+        max_retry_after=300,
+        sleep=sleeps.append,
+    )
+
+    assert completed.returncode == 1
+    assert attempts == [0]
+    assert sleeps == []

### Known Concerns
1. Retry is benchmark-runner scoped; the general agent still surfaces billing 402s.
2. Each retry gets the configured per-attempt timeout, so total instance wall can exceed --timeout by the settlement delay.
3. Stderr is captured per attempt before appending so an old retryable response cannot cause a later unrelated failure to retry.

### Specific Questions for Reviewer
1. Is runner-level retry the correct boundary for task 1465?
2. Are the reason/delay checks narrow and bounded enough?
3. Does retrying in the same container preserve fair benchmark state after a pre-inference rejection?
4. Are CLI controls and tests sufficient?

<!-- event id="review-1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. run_once opens the raw stdout file in append mode on retries but the surrounding pipeline (patch collection / transcript parsing) appears to assume a single attempt's output. If the first attempt wrote partial agent output before the 402, the retry's output is concatenated into the same raw file with no attempt delimiter (unlike stderr, which gets a '--- in-flight budget retry N ---' marker). This can corrupt transcript parsing or produce misleading raw logs. Either truncate stdout on retry (the failed attempt's stdout is preserved elsewhere or explicitly discarded) or write the same attempt-boundary marker to the raw file, and confirm downstream consumers tolerate multi-attempt content.
B2. The proposed contract says retry happens only for a 'pre-inference rejection', but nothing in the runner verifies the failed attempt made no mutating progress inside the container. run_once retries the same container after any non-zero exit whose stderr matches the retry pattern; if the agent partially modified /testbed before the provider error surfaced (e.g., budget exhaustion mid-run rather than pre-inference), the second attempt starts from dirty state, which biases benchmark results. Either reset the container/worktree state before retrying, or document and enforce (e.g., via a guard or explicit rationale in code comments/README) why the in_flight_budget_exhausted reason can only occur before any tool execution.
B3. The retry-eligibility check scans the entire accumulated stderr semantics only implicitly: current_error is per-attempt (good), but the regex matches the retryable reason anywhere in that attempt's stderr, including agent-echoed content such as a model transcript quoting a prior error or test output containing the literal string. A stray occurrence of '"reason":"in_flight_budget_exhausted"' plus any '"Retry-After":"N"' anywhere in stderr triggers a retry and a sleep of up to max_retry_after. Tighten the match: require both fields within the same JSON error object (or parse the openrouter 402 error line as JSON) rather than two independent regex hits across the whole stream.

### Non-Blocking Suggestions
S1. The per-attempt timeout means total wall time can reach (retries+1)*timeout plus settlement delays, but the timeout WARN message ('hit timeout ({timeout}s)') and downstream duration accounting only reflect the configured single-attempt timeout. Consider logging total elapsed time and attempt count in the per-instance JSON so cost/duration analysis distinguishes retried instances.
S2. in_flight_retry_after treats a Retry-After of 0 as non-retryable (0 < delay), which silently converts a valid 'retry immediately' hint into a hard failure. Consider clamping 0 to a small minimum delay (e.g., 1s) or documenting the choice.
S3. Add a test covering the multi-retry exhaustion path (max_retries=2, all attempts fail with retryable errors) to assert the final CompletedProcess is returned after exactly max_retries sleeps, and a test that returncode 124/137 (timeout kills) is never treated as retryable even if stale stderr text matches.
S4. The Retry-After regex only accepts integer seconds; RFC 7231 also allows HTTP-date values. If OpenRouter ever emits a date form, the retry silently degrades to no-retry. A brief comment noting the deliberate integer-only support would prevent future confusion.

**Verdict:** REVISE

<!-- event id="rebuttal-1" artifact path="1465-inflight-retry/artifacts/round-1-rebuttal.diff" sha256="c8a8e1337be0f600637c8dde0cc4268d49e1f0039c457216667e0867f7d9a2ea" -->
## Rebuttal — Round 1
**Position:** AGREE

### Responses to Blocking Concerns
Re B1: ACCEPT — Each retry now truncates raw stdout so downstream parsers see exactly one agent process. Rejected-attempt diagnostics remain in delimiter-separated stderr and the summary retry count.
Re B2: ACCEPT — Before every retry, the disposable testbed runs git reset --hard HEAD and git clean -fd. A cleanup failure aborts instead of running against dirty state; README documents this behavior.
Re B3: ACCEPT — Eligibility now requires the exact Daimonos OpenRouter 402 error-line prefix followed by one valid JSON object with numeric code 402, the exact metadata reason, a headers object, and its Retry-After value. Independent regex matches were removed.

### Revised Code / Diff
diff --git i/benchmarks/swebench/README.md w/benchmarks/swebench/README.md
index 44a610f..a8231b8 100644
--- i/benchmarks/swebench/README.md
+++ w/benchmarks/swebench/README.md
@@ -62,6 +62,16 @@ OPENROUTER_API_KEY=... .venv/bin/mini-extra swebench \
   --environment-class docker --workers 1 --output results/mini-<label>
 ```
 
+Docker runs retry OpenRouter's exact `in_flight_budget_exhausted` response once,
+after the response's own `Retry-After` delay. `--in-flight-retries` changes the
+attempt bound; `--max-retry-after` rejects unexpectedly long provider delays.
+Before retrying, the disposable testbed is reset and untracked files are
+removed, so a later-generation rejection cannot inherit partial edits. The
+final transcript replaces the rejected attempt's stdout; stderr retains each
+attempt boundary. Per-instance `wall_ms` includes settlement waits and
+`in_flight_retries` records the retry count. Other HTTP 402 billing failures are
+never retried.
+
 Each run writes `results/<run-id>/` with per-instance token/cost JSONs
 (same schema as the in-house suite — `../analyze.py results/` works),
 `.patch` files, raw transcripts, and `preds.jsonl`.
diff --git i/benchmarks/swebench/run_agent.py w/benchmarks/swebench/run_agent.py
index d0ea5d2..adadd19 100644
--- i/benchmarks/swebench/run_agent.py
+++ w/benchmarks/swebench/run_agent.py
@@ -23,6 +23,7 @@ import shutil
 import subprocess
 import sys
 import tempfile
+import time
 
 HERE = pathlib.Path(__file__).parent
 BENCH_ROOT = HERE.parent
@@ -149,7 +150,73 @@ def docker(args, **kw):
     return run_text(["docker", *args], **kw)
 
 
-def run_instance_docker(inst, run_dir, bench_env, model, timeout, keep):
+def in_flight_retry_after(error_text, max_retry_after):
+    prefix = "Error: agent error: openrouter 402 Payment Required: "
+    for line in error_text.splitlines():
+        if not line.startswith(prefix):
+            continue
+        try:
+            payload = json.loads(line.removeprefix(prefix))
+        except json.JSONDecodeError:
+            continue
+        if not isinstance(payload, dict):
+            continue
+        error = payload.get("error")
+        if not isinstance(error, dict):
+            continue
+        metadata = error.get("metadata")
+        if not isinstance(metadata, dict):
+            continue
+        if error.get("code") != 402:
+            continue
+        if metadata.get("reason") != "in_flight_budget_exhausted":
+            continue
+        # OpenRouter's embedded Retry-After is currently integer seconds. Do
+        # not guess at HTTP-date forms without an actual provider example.
+        headers = metadata.get("headers")
+        if not isinstance(headers, dict):
+            continue
+        delay = headers.get("Retry-After")
+        if isinstance(delay, str) and delay.isdigit():
+            delay = int(delay)
+        if isinstance(delay, int) and not isinstance(delay, bool):
+            return delay if 0 <= delay <= max_retry_after else None
+    return None
+
+
+def run_with_in_flight_retry(
+    run_once,
+    *,
+    max_retries,
+    max_retry_after,
+    sleep=time.sleep,
+):
+    attempt = 0
+    while True:
+        completed, error_text = run_once(attempt)
+        if completed.returncode in (0, 124, 137) or attempt >= max_retries:
+            return completed, attempt
+        delay = in_flight_retry_after(error_text, max_retry_after)
+        if delay is None:
+            return completed, attempt
+        attempt += 1
+        print(
+            "       WARN: OpenRouter in-flight budget is settling; "
+            f"retrying in {delay}s ({attempt}/{max_retries})"
+        )
+        sleep(delay)
+
+
+def run_instance_docker(
+    inst,
+    run_dir,
+    bench_env,
+    model,
+    timeout,
+    keep,
+    in_flight_retries,
+    max_retry_after,
+):
     iid = inst["instance_id"]
     image = inst.get("image")
     if not image:
@@ -185,16 +252,57 @@ def run_instance_docker(inst, run_dir, bench_env, model, timeout, keep):
     started = utcnow()
     t0 = dt.datetime.now()
     try:
-        with raw.open("w") as out, err.open("w") as errf:
-            proc = subprocess.run(
-                [
-                    "timeout", "--kill-after=10s", str(timeout),
-                    "docker", "exec", "-w", "/testbed",
-                    "-e", f"BENCH_MODEL={model}",
-                    cname, "bash", "-c", DOCKER_EXEC_SCRIPT,
-                ],
-                stdout=out, stderr=errf, check=False,
-            )
+        def run_once(attempt):
+            if attempt:
+                for cleanup_args in (
+                    ["exec", "-w", "/testbed", cname, "git", "reset", "--hard", "HEAD"],
+                    ["exec", "-w", "/testbed", cname, "git", "clean", "-fd"],
+                ):
+                    cleanup = docker(cleanup_args)
+                    if cleanup.returncode != 0:
+                        current_error = (
+                            "benchmark retry cleanup failed: "
+                            f"{cleanup.stderr or cleanup.stdout}"
+                        )
+                        with err.open("a") as errf:
+                            errf.write(
+                                f"\n--- in-flight budget retry {attempt} ---\n"
+                                f"{current_error}\n"
+                            )
+                        return (
+                            subprocess.CompletedProcess([], cleanup.returncode),
+                            current_error,
+                        )
+            # The rejected attempt is represented by stderr and retry_count.
+            # Keep only the successful/final transcript so JSON-event parsers
+            # never see concatenated agent processes.
+            with raw.open("w") as out:
+                completed = subprocess.run(
+                    [
+                        "timeout", "--kill-after=10s", str(timeout),
+                        "docker", "exec", "-w", "/testbed",
+                        "-e", f"BENCH_MODEL={model}",
+                        cname, "bash", "-c", DOCKER_EXEC_SCRIPT,
+                    ],
+                    stdout=out,
+                    stderr=subprocess.PIPE,
+                    text=True,
+                    encoding="utf-8",
+                    errors="replace",
+                    check=False,
+                )
+            current_error = completed.stderr or ""
+            with err.open("w" if attempt == 0 else "a") as errf:
+                if attempt:
+                    errf.write(f"\n--- in-flight budget retry {attempt} ---\n")
+                errf.write(current_error)
+            return completed, current_error
+
+        proc, retry_count = run_with_in_flight_retry(
+            run_once,
+            max_retries=in_flight_retries,
+            max_retry_after=max_retry_after,
+        )
         proc_rc = proc.returncode
         if proc_rc in (124, 137):
             print(f"       WARN: {iid} hit timeout ({timeout}s) — killed")
@@ -233,6 +341,7 @@ def run_instance_docker(inst, run_dir, bench_env, model, timeout, keep):
     data["patch_bytes"] = len(patch)
     data["empty_patch"] = not patch.strip()
     data["runner_mode"] = "docker"
+    data["in_flight_retries"] = retry_count
     summary.write_text(json.dumps(data, indent=2))
     if not keep:
         shutil.rmtree(benchdir, ignore_errors=True)
@@ -315,6 +424,18 @@ def main():
     ap.add_argument("--tag", default="", help="label folded into run dir name")
     ap.add_argument("--model", default=None, help="override agent.env model")
     ap.add_argument("--timeout", type=int, default=900, help="per-instance seconds")
+    ap.add_argument(
+        "--in-flight-retries",
+        type=int,
+        default=1,
+        help="retries after OpenRouter in-flight-budget settlement responses",
+    )
+    ap.add_argument(
+        "--max-retry-after",
+        type=int,
+        default=300,
+        help="maximum accepted OpenRouter Retry-After delay in seconds",
+    )
     ap.add_argument("--keep", action="store_true", help="keep worktrees after run")
     ap.add_argument(
         "--docker", action="store_true",
@@ -322,6 +443,10 @@ def main():
              "(runnable test env; requires the musl daimonos build)",
     )
     args = ap.parse_args()
+    if args.in_flight_retries < 0:
+        ap.error("--in-flight-retries must be non-negative")
+    if args.max_retry_after <= 0:
+        ap.error("--max-retry-after must be positive")
 
     if not INSTANCES.exists():
         sys.exit("instances.jsonl missing — run fetch_dataset.py first")
@@ -374,12 +499,23 @@ def main():
 
     preds_path = run_dir / "preds.jsonl"
     try:
-        runner = run_instance_docker if args.docker else run_instance
         with preds_path.open("w") as preds:
             for inst in instances:
-                patch = runner(
-                    inst, run_dir, bench_env, model, args.timeout, args.keep
-                )
+                if args.docker:
+                    patch = run_instance_docker(
+                        inst,
+                        run_dir,
+                        bench_env,
+                        model,
+                        args.timeout,
+                        args.keep,
+                        args.in_flight_retries,
+                        args.max_retry_after,
+                    )
+                else:
+                    patch = run_instance(
+                        inst, run_dir, bench_env, model, args.timeout, args.keep
+                    )
                 preds.write(json.dumps({
                     "instance_id": inst["instance_id"],
                     "model_name_or_path": f"daimonos-{model}",
diff --git i/tests/test_swebench_runner.py w/tests/test_swebench_runner.py
index 301279d..a16cfe2 100644
--- i/tests/test_swebench_runner.py
+++ w/tests/test_swebench_runner.py
@@ -7,6 +7,8 @@ import subprocess
 import sys
 from pathlib import Path
 
+import pytest
+
 
 RUNNER = (
     Path(__file__).resolve().parents[1] / "benchmarks" / "swebench" / "run_agent.py"
@@ -56,3 +58,141 @@ def test_collect_patch_encodes_binary_files(tmp_path):
 
     assert "GIT binary patch" in patch
     assert "\ufffd" not in patch
+
+
+def test_in_flight_budget_retries_after_provider_settlement_delay():
+    runner = load_runner()
+    responses = iter(
+        [
+            (
+                subprocess.CompletedProcess([], 1),
+                'Error: agent error: openrouter 402 Payment Required: '
+                '{"error":{"message":"settling","code":402,"metadata":{"reason":'
+                '"in_flight_budget_exhausted","headers":{"Retry-After":"120"}}}}',
+            ),
+            (subprocess.CompletedProcess([], 0), ""),
+        ]
+    )
+    attempts = []
+    sleeps = []
+
+    def run_once(attempt):
+        attempts.append(attempt)
+        return next(responses)
+
+    completed, retry_count = runner.run_with_in_flight_retry(
+        run_once,
+        max_retries=1,
+        max_retry_after=300,
+        sleep=sleeps.append,
+    )
+
+    assert completed.returncode == 0
+    assert retry_count == 1
+    assert attempts == [0, 1]
+    assert sleeps == [120]
+
+
+@pytest.mark.parametrize(
+    "error_text",
+    [
+        'Error: agent error: openrouter 402 Payment Required: '
+        '{"error":{"message":"insufficient credits","code":402,"metadata":'
+        '{"headers":{"Retry-After":"120"}}}}',
+        'Error: agent error: openrouter 402 Payment Required: '
+        '{"error":{"code":402,"metadata":{"reason":'
+        '"in_flight_budget_exhausted"}}}',
+        'Error: agent error: openrouter 402 Payment Required: '
+        '{"error":{"code":402,"metadata":{"reason":'
+        '"in_flight_budget_exhausted","headers":{"Retry-After":"999"}}}}',
+        'Error: agent error: openrouter 402 Payment Required: '
+        '{"error":{"metadata":{"reason":"in_flight_budget_exhausted"}}} '
+        '{"headers":{"Retry-After":"120"}}',
+    ],
+)
+def test_other_billing_errors_and_unsafe_delays_are_not_retried(error_text):
+    runner = load_runner()
+    attempts = []
+    sleeps = []
+
+    def run_once(attempt):
+        attempts.append(attempt)
+        return subprocess.CompletedProcess([], 1), error_text
+
+    completed, retry_count = runner.run_with_in_flight_retry(
+        run_once,
+        max_retries=1,
+        max_retry_after=300,
+        sleep=sleeps.append,
+    )
+
+    assert completed.returncode == 1
+    assert retry_count == 0
+    assert attempts == [0]
+    assert sleeps == []
+
+
+def test_in_flight_retry_exhaustion_is_bounded():
+    runner = load_runner()
+    error = (
+        'Error: agent error: openrouter 402 Payment Required: '
+        '{"error":{"code":402,"metadata":{"reason":'
+        '"in_flight_budget_exhausted","headers":{"Retry-After":"1"}}}}'
+    )
+    attempts = []
+    sleeps = []
+
+    def run_once(attempt):
+        attempts.append(attempt)
+        return subprocess.CompletedProcess([], 1), error
+
+    completed, retry_count = runner.run_with_in_flight_retry(
+        run_once,
+        max_retries=2,
+        max_retry_after=300,
+        sleep=sleeps.append,
+    )
+
+    assert completed.returncode == 1
+    assert retry_count == 2
+    assert attempts == [0, 1, 2]
+    assert sleeps == [1, 1]
+
+
+def test_in_flight_retry_accepts_immediate_provider_hint():
+    runner = load_runner()
+    error = (
+        'Error: agent error: openrouter 402 Payment Required: '
+        '{"error":{"code":402,"metadata":{"reason":'
+        '"in_flight_budget_exhausted","headers":{"Retry-After":"0"}}}}'
+    )
+
+    assert runner.in_flight_retry_after(error, max_retry_after=300) == 0
+
+
+@pytest.mark.parametrize("returncode", [124, 137])
+def test_in_flight_retry_does_not_retry_killed_attempts(returncode):
+    runner = load_runner()
+    error = (
+        'Error: agent error: openrouter 402 Payment Required: '
+        '{"error":{"code":402,"metadata":{"reason":'
+        '"in_flight_budget_exhausted","headers":{"Retry-After":"120"}}}}'
+    )
+    attempts = []
+    sleeps = []
+
+    def run_once(attempt):
+        attempts.append(attempt)
+        return subprocess.CompletedProcess([], returncode), error
+
+    completed, retry_count = runner.run_with_in_flight_retry(
+        run_once,
+        max_retries=2,
+        max_retry_after=300,
+        sleep=sleeps.append,
+    )
+
+    assert completed.returncode == returncode
+    assert retry_count == 0
+    assert attempts == [0]
+    assert sleeps == []

### New Points
R1. Applied all suggestions: summary records in_flight_retries and wall_ms includes waits; Retry-After 0 is accepted; exhaustion and timeout-kill behavior are tested; integer-seconds-only support is documented in code.

**Requesting:** RE-REVIEW

<!-- event id="review-2" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S5. Minor: on the retry-cleanup failure path, run_once returns subprocess.CompletedProcess([], cleanup.returncode) whose returncode could theoretically be 0 if the docker helper reported failure through stdout/stderr conventions rather than exit code; consider normalizing to a fixed nonzero sentinel (e.g., 1) so the outer loop can never misread a cleanup failure as success. Not blocking because docker() returncode != 0 is the guard, so the propagated code is nonzero under current logic.
S6. Minor: the git reset/clean cleanup only restores /testbed tracked and untracked files; agent-created state outside /testbed (e.g., caches or files in $HOME inside the container) is not reset. Given the retryable error is a pre-inference rejection this is unlikely to matter, but a one-line README caveat would make the fairness boundary explicit.

### Responses to Prior Points
Re R1: Verified against the revised diff. S1: retry_count is recorded as in_flight_retries in the per-instance summary JSON and README states wall_ms includes settlement waits (t0 spans the retry loop, so sleeps are included). S2: Retry-After of 0 is now accepted (0 <= delay <= max_retry_after) with a dedicated test asserting in_flight_retry_after returns 0. S3: exhaustion path is covered (max_retries=2, attempts [0,1,2], sleeps [1,1]) and returncodes 124/137 are short-circuited in run_with_in_flight_retry before eligibility parsing, with parametrized tests confirming no retry even when stderr matches. S4: integer-seconds-only support is documented via an inline comment declining to guess at HTTP-date forms. All suggestions satisfactorily applied.
B1: resolved
B2: resolved
B3: resolved

**Verdict:** APPROVE
