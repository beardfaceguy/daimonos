# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1468-nonutf8-patch" -->

<!-- event id="request" artifact path="1468-nonutf8-patch/artifacts/round-1-review-request.diff" sha256="db54ba6723c2c4fea4a72f72f2f2c0cb823b236b75be998c6761be654bb3596a" -->
## Review Request — Round 1
**Task:** 1468 — Make SWE-bench patch collection robust to non-UTF-8 artifacts
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Route captured text subprocesses through a UTF-8 decoder with replacement for diagnostics and request git --binary patches in both local and Docker paths, preventing paid runs from aborting on binary artifacts while preserving applyable binary diffs.

### Relevant Code / Diff
diff --git i/benchmarks/swebench/run_agent.py w/benchmarks/swebench/run_agent.py
index 1b78520..d0ea5d2 100644
--- i/benchmarks/swebench/run_agent.py
+++ w/benchmarks/swebench/run_agent.py
@@ -52,8 +52,19 @@ external test harness.
 """
 
 
+def run_text(args, **kw):
+    return subprocess.run(
+        args,
+        capture_output=True,
+        text=True,
+        encoding="utf-8",
+        errors="replace",
+        **kw,
+    )
+
+
 def sh(args, **kw):
-    return subprocess.run(args, check=True, capture_output=True, text=True, **kw)
+    return run_text(args, check=True, **kw)
 
 
 def utcnow() -> str:
@@ -100,13 +111,13 @@ def collect_patch(workdir: pathlib.Path) -> str:
     sh(["git", "-C", str(workdir), "add", "-N", "."])
     # Force standard a/-b/ prefixes: the SWE-bench applier rejects the
     # mnemonic i/-w/ prefixes a user gitconfig may enable.
-    return subprocess.run(
+    return run_text(
         [
             "git", "-C", str(workdir),
             "-c", "diff.noprefix=false", "-c", "diff.mnemonicPrefix=false",
-            "diff", "--src-prefix=a/", "--dst-prefix=b/",
+            "diff", "--binary", "--src-prefix=a/", "--dst-prefix=b/",
         ],
-        check=True, capture_output=True, text=True,
+        check=True,
     ).stdout
 
 
@@ -135,7 +146,7 @@ exec daimonos --debug-tokens -w /testbed agent "$(cat /bench/prompt.txt)" \
 
 
 def docker(args, **kw):
-    return subprocess.run(["docker", *args], capture_output=True, text=True, **kw)
+    return run_text(["docker", *args], **kw)
 
 
 def run_instance_docker(inst, run_dir, bench_env, model, timeout, keep):
@@ -200,7 +211,7 @@ def run_instance_docker(inst, run_dir, bench_env, model, timeout, keep):
         patch = docker([
             "exec", "-w", "/testbed", cname, "git",
             "-c", "diff.noprefix=false", "-c", "diff.mnemonicPrefix=false",
-            "diff", "--src-prefix=a/", "--dst-prefix=b/",
+            "diff", "--binary", "--src-prefix=a/", "--dst-prefix=b/",
         ]).stdout
     finally:
         docker(["rm", "-f", cname])
diff --git 1/tests/test_swebench_runner.py 2/tests/test_swebench_runner.py
new file mode 100644
index 0000000..301279d
--- /dev/null
+++ 2/tests/test_swebench_runner.py
@@ -0,0 +1,58 @@
+"""Tests for the Daimonos SWE-bench runner."""
+
+from __future__ import annotations
+
+import importlib.util
+import subprocess
+import sys
+from pathlib import Path
+
+
+RUNNER = (
+    Path(__file__).resolve().parents[1] / "benchmarks" / "swebench" / "run_agent.py"
+)
+
+
+def load_runner():
+    spec = importlib.util.spec_from_file_location("swebench_run_agent", RUNNER)
+    assert spec is not None and spec.loader is not None
+    module = importlib.util.module_from_spec(spec)
+    spec.loader.exec_module(module)
+    return module
+
+
+def test_run_text_replaces_non_utf8_subprocess_output():
+    runner = load_runner()
+
+    completed = runner.run_text(
+        [sys.executable, "-c", "import sys; sys.stdout.buffer.write(b'\\xb6')"]
+    )
+
+    assert completed.returncode == 0
+    assert completed.stdout == "\ufffd"
+
+
+def test_collect_patch_encodes_binary_files(tmp_path):
+    runner = load_runner()
+    subprocess.run(["git", "init", "-q", str(tmp_path)], check=True)
+    subprocess.run(
+        ["git", "-C", str(tmp_path), "config", "user.email", "test@example.com"],
+        check=True,
+    )
+    subprocess.run(
+        ["git", "-C", str(tmp_path), "config", "user.name", "Test"],
+        check=True,
+    )
+    tracked = tmp_path / "tracked.txt"
+    tracked.write_text("tracked\n")
+    subprocess.run(["git", "-C", str(tmp_path), "add", "tracked.txt"], check=True)
+    subprocess.run(
+        ["git", "-C", str(tmp_path), "commit", "-qm", "fixture"],
+        check=True,
+    )
+    (tmp_path / "artifact.bin").write_bytes(b"\x00\xb6")
+
+    patch = runner.collect_patch(tmp_path)
+
+    assert "GIT binary patch" in patch
+    assert "\ufffd" not in patch

### Known Concerns
1. Replacement decoding could corrupt an undetected non-UTF-8 text diff, but --binary protects Git-detected binary files.
2. The failed paid Sphinx turn cannot be reconstructed because its container was removed.

### Specific Questions for Reviewer
1. Does this prevent the observed UnicodeDecodeError without hiding subprocess failures?
2. Are binary patches applyable by SWE-bench?
3. Do the tests cover the relevant boundaries?

<!-- event id="review-1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S1. Consider logging a warning when replacement characters (U+FFFD) appear in a collected patch or in subprocess diagnostics, so that a silently mangled non-UTF-8 text diff (e.g., latin-1 source files, which git does not classify as binary) is detectable in run artifacts rather than only surfacing as a downstream apply failure.
S2. The Docker path's patch collection (git diff via `docker exec`) is only covered indirectly by the run_text test; a small test asserting the docker() wrapper routes through run_text (e.g., via monkeypatching subprocess.run) would guard against future regressions reintroducing raw text=True decoding.
S3. Consider asserting in test_collect_patch_encodes_binary_files that the emitted patch actually applies cleanly (e.g., `git apply --check` in a fresh clone) to directly answer the 'applyable by SWE-bench' question, since 'GIT binary patch' presence alone does not verify applicability with a/-b/ prefixes.

**Verdict:** APPROVE
