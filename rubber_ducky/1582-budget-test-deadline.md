# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="budget-test-deadline-1582-853a55b" -->

<!-- event id="request" artifact path="1582-budget-test-deadline/artifacts/round-1-review-request.diff" sha256="29be57f990f236e7bf2e272da206d5913cc53bf8384e6e5870b0bc019c2373a3" -->
## Review Request — Round 1
**Task:** 1582 — Test fix: the files-search deadline test must not depend on a 1 ms race (PR #289)
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Review the diff origin/master (00d4235)..853a55b of PR #289. It changes one test in tests/test_tool_budgets.py and no product code. Context: WP18 (cooperative tool budgets) is on master. `--tool-timeout-ms N` gives each top-level tool call a deadline. Two layers enforce it for the files-mode (index) search: ops/mod.rs checks session.budget_check_deadline('search') before and after the index search, and mcp::dispatch_tool_inner checks the deadline after each dispatched tool and replaces any result by a tool_timeout failure when the deadline has passed. batch and execute_script share one budget (one absolute deadline) with their children. Problem: test_timeout_file_search_returns_timeout started the server with --tool-timeout-ms 1 and required isError with code tool_timeout from search {pattern, mode: files}. The index search over 20 small files can finish, including the check after it, in under 1 ms. Then the call succeeds, which is correct product behavior, and the test fails. Measured on the master tree on an idle host: 8 failures in 10 runs; in one process 20 of 20 calls succeeded. It passed on the same host under load and passes in CI. Change: the server gets --tool-timeout-ms 300. A control first runs the search alone inside a batch and requires success with the expected file in the result. Then a batch with two children runs: exec sleep 30, then the files search. The children share one 300 ms deadline; exec is stopped at the deadline with tool_timeout; the search starts after the deadline and must return a failure that contains tool_timeout and does not contain a result file name. Verification by the worker: the test had 0 failures in 12 idle runs and in 8 runs with 40 busy CPU processes. tests/test_tool_budgets.py: 49 passed. Mutation 1: with the three budget_check_deadline('search') calls in ops/mod.rs disabled, the test still PASSES, because the check in mcp::dispatch_tool_inner reclassifies the late result. Mutation 2: with those calls disabled AND the dispatch check removed, the test FAILS (the search returns its results after the deadline). So the test proves the caller-visible rule (a files search after the deadline returns tool_timeout and no result) and does not prove which layer enforces it. CI on the PR was running at the time of this request.

### Relevant Code / Diff
diff --git a/tests/test_tool_budgets.py b/tests/test_tool_budgets.py
index 1975f5f..56978f5 100644
--- a/tests/test_tool_budgets.py
+++ b/tests/test_tool_budgets.py
@@ -778,16 +778,36 @@ def test_timeout_content_search_returns_timeout(daimonos_factory, tmp_path):
 
 def test_timeout_file_search_returns_timeout(daimonos_factory, tmp_path):
     """F2 (M2): the index (files-mode) search checks the deadline before the
-    result is built, so a 1 ms deadline returns ``tool_timeout``."""
+    result is built.
+
+    The deadline must have passed for certain when the search runs. A 1 ms
+    deadline does not give that: the index search can end in less than 1 ms on
+    an idle host, and it then succeeds, which is correct. So the search is the
+    second child of a batch. The children share one deadline of 300 ms. The
+    first child sleeps and is stopped at that deadline, so the search starts
+    after it.
+    """
     ws = tmp_path / "ws"
     ws.mkdir()
     for i in range(20):
         (ws / f"wp18f_{i}.rs").write_text("x")
-    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1"])
-    result = client.call_tool("search", {"pattern": "wp18f", "mode": "files"})
-    assert result.get("isError") is True, result
-    data = json.loads(_text(result))
-    assert data["code"] == "tool_timeout", data
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "300"])
+    search = {"tool": "search", "arguments": {"pattern": "wp18f", "mode": "files"}}
+
+    # Control: with time left, the same search succeeds.
+    alone = json.loads(_text(client.call_tool("batch", {"ops": [search]})))
+    assert [child["ok"] for child in alone] == [True], alone
+    assert "wp18f_0.rs" in json.dumps(alone[0])
+
+    result = client.call_tool("batch", {"ops": [
+        {"tool": "exec", "arguments": {"command": "sleep", "args": ["30"]}},
+        search,
+    ]})
+    children = json.loads(_text(result))
+    assert [child["ok"] for child in children] == [False, False], children
+    late = json.dumps(children[1])
+    assert "tool_timeout" in late, late
+    assert "wp18f_0.rs" not in late, "no late result on a deadline hit"
 
 
 def test_ac_19_1_large_tree_search_times_out_within_bound(daimonos_factory, tmp_path):

### Known Concerns
1. The test no longer exercises a top-level files search with a passed deadline; it exercises a files search as a batch child. The batch handler dispatches children through the same dispatch function, but the path is not identical to a top-level call.
2. The test cannot distinguish the two enforcement layers (mutation 1 passes). A per-layer test would need a Rust unit test on ops::dispatch with a budget whose deadline already passed.
3. The control and the main batch use a 300 ms deadline. On a very slow runner, the control (a search alone) could pass 300 ms and fail; the first child of the main batch needs the process start of `sleep` within the deadline, but a failure to start in time still gives tool_timeout for both children.
4. Four other tests keep --tool-timeout-ms 1 (read_file of a 100 KB file, ls of 20 files, content search of 20 files, the git plugin). Each had 0 failures in 10 idle runs on the worker host. They have the same race in principle and were not changed.
5. The assertion that no result leaked checks that the string 'wp18f_0.rs' is not in the JSON of the second child. The timeout failure names its tool and limit; it is not checked field by field.

### Specific Questions for Reviewer
1. Does the new test prove the stated rule, or can it pass for a wrong reason?
2. Is the test deterministic enough for a loaded CI runner and for a fast idle host?
3. Does the change lose coverage that the old test had and that matters?

<!-- event id="review-1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. Coverage regression on the top-level path: the old test exercised a top-level `search` call with a passed deadline; the new test only exercises the search as a batch child. The worker's own known concern notes the paths are not identical. Since mutation testing shows the batch-child path is satisfied by the dispatch_tool_inner reclassification alone, there is no remaining test that a TOP-LEVEL files search with an expired deadline returns tool_timeout. Add a top-level variant that guarantees an expired deadline deterministically (e.g., a Rust unit test on ops::dispatch with an already-passed budget deadline, as the worker suggests, or an equivalent integration construction), or explain concretely why the batch-child dispatch path provably covers the top-level path.
B2. The assertion `assert "tool_timeout" in late` on the raw JSON of the second child is too loose: it can pass for a wrong reason if the string 'tool_timeout' appears anywhere in the serialized child (e.g., in an unrelated message, echoed arguments, or an error from a different cause such as the exec failure text being included). Combined with only checking that 'wp18f_0.rs' is absent, the test does not actually verify that the search child failed WITH code tool_timeout. Parse the child and assert the structured error code field (as the old test did with data["code"] == "tool_timeout"), so the test proves the stated caller-visible rule rather than a substring coincidence.

### Non-Blocking Suggestions
S1. Harden the control against very slow runners: the control batch search must finish within 300 ms or the test fails spuriously. Either use a larger timeout for the control run (separate server invocation) or allow a generous deadline (e.g., several seconds) for the control while keeping the main batch deadline small enough to be fast. Alternatively, document in the test why 300 ms is judged safe (e.g., margin measured under 40 busy CPU processes).
S2. The four other tests that keep --tool-timeout-ms 1 (read_file, ls, content search, git plugin) have the same race in principle. Consider filing a follow-up to migrate them to the same deterministic construction before they start flaking on faster hosts, and note the ticket in the PR description.
S3. Also assert the first child's failure is specifically tool_timeout (structured code check), not merely ok == False, so a failure of `sleep` to start (e.g., missing binary) is distinguishable from the intended deadline stop and the test's precondition — that the deadline has certainly passed when the search runs — is actually verified.

**Verdict:** REVISE

<!-- event id="rebuttal-1" artifact path="1582-budget-test-deadline/artifacts/round-1-rebuttal.diff" sha256="70793ed3e1857e53c6e8d92fc250fb794c1b4e3f4941fc9c1721362d62c424cc" -->
## Rebuttal — Round 1
**Position:** AGREE

### Responses to Blocking Concerns
Re B1: ACCEPT — Accepted: the first version lost the top-level case and could not see the check inside the search. Added a Rust test module in src/ops/mod.rs. ops::tests::a_files_search_after_the_deadline_is_a_timeout builds a Session with a WorkspaceIndex over 20 files and tool_budget.timeout_ms = 1, installs the budget with set_operation_budget, waits 20 ms, asserts budget.check_deadline() is an error (the deadline passed for certain), and then calls ops::dispatch with a top-level Request::Single FIND op, which is the request that the `search` tool builds for mode files. It requires a failed response whose typed data has code tool_timeout and tool search, has no results field, and holds no file name. ops::dispatch is below mcp::dispatch_tool_inner, so the general check of the MCP seam cannot satisfy this test: only the check inside the search can. Mutation: with the three budget_check_deadline('search') calls in ops/mod.rs disabled, this test FAILS (the mutation that the Python test could not detect). Control ops::tests::a_files_search_with_time_left_returns_its_results: the same op with a 60 s deadline succeeds, returns wp18f_0.rs, and leaves no budget on the session. Both tests: 20 of 20 idle runs and 10 of 10 runs with 40 busy CPU processes pass.
Re B2: ACCEPT — Accepted: a substring match on the serialized child could pass for a wrong reason. The Python test now parses the failure object of each batch child (json.loads(child['error'])) and compares the code field: late['code'] == 'tool_timeout', late['tool'] == 'search', no 'results' key, and no 'wp18f_' text in the error. Per S3, it also checks the precondition in the same way: the first child (exec sleep 30) must have code tool_timeout, so a sleep that failed for another reason does not pass, and the test order asserts [exec, search]. Per S1, the control uses a second server with --tool-timeout-ms 60000, so a slow runner cannot fail the control on a 300 ms limit; the main batch keeps 300 ms. The Python test had 0 failures in 10 idle runs after this change.

### Revised Code / Diff
diff --git a/src/ops/mod.rs b/src/ops/mod.rs
index 235e8b9..6399c9d 100644
--- a/src/ops/mod.rs
+++ b/src/ops/mod.rs
@@ -361,3 +361,80 @@ fn session_info(session: &Session) -> Response {
 
     Response::ok(info)
 }
+
+#[cfg(test)]
+mod tests {
+    use super::*;
+    use crate::config::Config;
+    use std::sync::Arc;
+
+    fn files_search(pattern: &str) -> Request {
+        Request::Single(Op {
+            c: protocol::op::FIND,
+            p: Some(pattern.to_string()),
+            ..Op::default()
+        })
+    }
+
+    /// A session with an index over 20 small files and a 1 ms tool deadline.
+    fn indexed_session(dir: &std::path::Path) -> Session {
+        for i in 0..20 {
+            std::fs::write(dir.join(format!("wp18f_{i}.rs")), "x").unwrap();
+        }
+        let mut cfg = Config::default();
+        cfg.tool_budget.timeout_ms = Some(1);
+        let cfg = Arc::new(cfg);
+        let mut session = Session::new(dir.to_path_buf(), Arc::clone(&cfg));
+        session.index = Some(Arc::new(crate::index::WorkspaceIndex::new(
+            dir.to_path_buf(),
+            &cfg.index,
+            false,
+        )));
+        session
+    }
+
+    /// FR-19 (WP18): a top-level files-mode search whose deadline passed is a
+    /// timeout with no result. The check is in the search itself: this test
+    /// calls `ops::dispatch`, below the general check of the MCP dispatch.
+    ///
+    /// The budget is installed first and the test then waits, so the deadline
+    /// has passed for certain. A deadline that is only short (1 ms) is not
+    /// enough: the index search can end before it.
+    #[tokio::test]
+    async fn a_files_search_after_the_deadline_is_a_timeout() {
+        let dir = tempfile::tempdir().unwrap();
+        let mut session = indexed_session(dir.path());
+
+        let budget = session.tool_budget_config.start().expect("a budget");
+        session.set_operation_budget(Some(Arc::clone(&budget)));
+        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
+        assert!(budget.check_deadline().is_err(), "the deadline passed");
+
+        let response = dispatch(&mut session, files_search("wp18f")).await;
+        assert!(!response.ok, "{response:?}");
+        let failure = response.d.clone().expect("a typed failure");
+        assert_eq!(failure["code"], "tool_timeout", "{failure}");
+        assert_eq!(failure["tool"], "search", "{failure}");
+        assert!(failure.get("results").is_none(), "{failure}");
+        assert!(!format!("{response:?}").contains("wp18f_"), "{response:?}");
+        // The call did not install this budget, so it does not clear it.
+        assert!(session.tool_budget.is_some());
+    }
+
+    /// Control: the same search without an installed budget gets a new one and
+    /// can succeed. With a long deadline it returns the files.
+    #[tokio::test]
+    async fn a_files_search_with_time_left_returns_its_results() {
+        let dir = tempfile::tempdir().unwrap();
+        let mut session = indexed_session(dir.path());
+        session.tool_budget_config.timeout_ms = Some(60_000);
+
+        let response = dispatch(&mut session, files_search("wp18f")).await;
+        assert!(response.ok, "{response:?}");
+        assert!(
+            format!("{response:?}").contains("wp18f_0.rs"),
+            "{response:?}"
+        );
+        assert!(session.tool_budget.is_none(), "the call cleared its budget");
+    }
+}
diff --git a/tests/test_tool_budgets.py b/tests/test_tool_budgets.py
index 1975f5f..3fcd324 100644
--- a/tests/test_tool_budgets.py
+++ b/tests/test_tool_budgets.py
@@ -776,18 +776,55 @@ def test_timeout_content_search_returns_timeout(daimonos_factory, tmp_path):
     assert "matches" not in data, "no late matches on a deadline hit"
 
 
+def _child_failure(child):
+    """The parsed failure object of a failed batch child."""
+    assert child["ok"] is False, child
+    return json.loads(child["error"])
+
+
 def test_timeout_file_search_returns_timeout(daimonos_factory, tmp_path):
-    """F2 (M2): the index (files-mode) search checks the deadline before the
-    result is built, so a 1 ms deadline returns ``tool_timeout``."""
+    """F2 (M2): a files-mode (index) search that runs after the deadline
+    returns ``tool_timeout`` and no result.
+
+    The deadline must have passed for certain when the search runs. A 1 ms
+    deadline does not give that: the index search can end in less than 1 ms on
+    an idle host, and it then succeeds, which is correct. So the search is the
+    second child of a batch. The children share one deadline of 300 ms. The
+    first child sleeps and is stopped at that deadline, so the search starts
+    after it.
+
+    The Rust test ``ops::tests::a_files_search_after_the_deadline_is_a_timeout``
+    covers a top-level search and the check inside the search itself.
+    """
     ws = tmp_path / "ws"
     ws.mkdir()
     for i in range(20):
         (ws / f"wp18f_{i}.rs").write_text("x")
-    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1"])
-    result = client.call_tool("search", {"pattern": "wp18f", "mode": "files"})
-    assert result.get("isError") is True, result
-    data = json.loads(_text(result))
-    assert data["code"] == "tool_timeout", data
+    search = {"tool": "search", "arguments": {"pattern": "wp18f", "mode": "files"}}
+
+    # Control: with time left, the same search in a batch succeeds. The deadline
+    # is long, so a slow runner does not fail the control.
+    relaxed = daimonos_factory(ws, args=["--tool-timeout-ms", "60000"])
+    alone = json.loads(_text(relaxed.call_tool("batch", {"ops": [search]})))
+    assert [child["ok"] for child in alone] == [True], alone
+    assert "wp18f_0.rs" in json.dumps(alone[0])
+
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "300"])
+    result = client.call_tool("batch", {"ops": [
+        {"tool": "exec", "arguments": {"command": "sleep", "args": ["30"]}},
+        search,
+    ]})
+    children = json.loads(_text(result))
+    assert [child["tool"] for child in children] == ["exec", "search"], children
+    # Precondition: the sleep was stopped by the deadline, not by another error.
+    # So the deadline passed before the search started.
+    stopped = _child_failure(children[0])
+    assert stopped["code"] == "tool_timeout", stopped
+    late = _child_failure(children[1])
+    assert late["code"] == "tool_timeout", late
+    assert late.get("tool") == "search", late
+    assert "results" not in late, "no late result on a deadline hit"
+    assert "wp18f_" not in children[1]["error"]
 
 
 def test_ac_19_1_large_tree_search_times_out_within_bound(daimonos_factory, tmp_path):

### New Points
R1. S2 applied: follow-up task Vikunja #1626 for the four other tests that keep --tool-timeout-ms 1 (read_file, ls, content search, git plugin). They were not changed in this PR. Each had 0 failures in 10 idle runs on the worker host.
R2. Verification on the revised head: fmt and clippy exit 0. tests/test_tool_budgets.py: 49 passed. The two new Rust tests pass. The complete Rust and pytest suites were not run again on this head on the worker host; CI runs them on the PR.

**Requesting:** RE-REVIEW

<!-- event id="review-2" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
None.

### Responses to Prior Points
Re B1: The ACCEPT resolves the concern. The new Rust test ops::tests::a_files_search_after_the_deadline_is_a_timeout calls ops::dispatch directly with a top-level FIND request after deterministically expiring the budget (install budget, sleep 20 ms, assert check_deadline() is an error). Because ops::dispatch sits below mcp::dispatch_tool_inner, the MCP-seam reclassification cannot satisfy it, and the mutation evidence confirms it: disabling the three budget_check_deadline('search') calls now makes this test FAIL, exactly the mutation the Python test could not detect. The control test with a 60 s deadline guards against the test passing for an unrelated failure. Top-level coverage and layer attribution are restored; B1 is resolved.
Re B2: The ACCEPT resolves the concern. The Python test now parses each failed child's structured failure object (_child_failure via json.loads(child['error'])) and asserts late['code'] == 'tool_timeout' and late['tool'] == 'search', plus absence of a 'results' key and of 'wp18f_' text in the error. The precondition is also verified structurally per S3: the exec child must fail with code tool_timeout, so a sleep that failed for another reason cannot fake the deadline having passed. The substring-coincidence failure mode is gone; B2 is resolved.
Re R1: Acknowledged. Filing Vikunja #1626 for the four remaining --tool-timeout-ms 1 tests addresses S2 adequately; leaving them unchanged in this PR is a reasonable scoping decision since they showed 0 failures in 10 idle runs and the migration pattern is now established by this fix.
Re R2: Acknowledged. fmt/clippy clean, the targeted suite (49 passed) and both new Rust tests passing, plus the mutation and load-run evidence, are sufficient verification for a test-only change. Relying on CI for the full Rust and pytest suites on this head is acceptable; merge should wait for the green CI run as usual.
B1: resolved
B2: resolved

**Verdict:** APPROVE
